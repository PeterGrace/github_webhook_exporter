use std::fmt;

use sqlx::SqlitePool;
use thiserror::Error;
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime, UtcOffset};

use crate::{
    domain::{merge_queue::PullRequestNumber, repository::RepositoryId},
    telemetry::trace::{self, CommitSha, DatabaseOperation},
};

use super::sqlite_is_busy_or_locked;

const PULL_REQUEST_COMMIT_PRUNE_BATCH_SIZE: i64 = 1_000;
const PULL_REQUEST_COMMIT_TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// Durable commit-to-pull-request map built from `pull_request` deliveries.
///
/// A run whose payload names no pull request is attributed through its head commit. A commit seen
/// on two pull requests keeps the later one.
#[derive(Clone)]
pub(crate) struct PullRequestCommitStore {
    pool: SqlitePool,
}

impl PullRequestCommitStore {
    /// Creates a pull-request commit store from an already migrated SQLite pool.
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Maps `commit_sha` to `pull_request`, replacing any earlier mapping.
    ///
    /// # Errors
    ///
    /// Returns [`PullRequestCommitStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`PullRequestCommitStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn record(
        &self,
        repository_id: RepositoryId,
        commit_sha: &CommitSha,
        pull_request: PullRequestNumber,
    ) -> Result<(), PullRequestCommitStoreError> {
        trace::instrument_database_operation(DatabaseOperation::PullRequestCommitRecord, async {
            sqlx::query(
                "INSERT INTO pull_request_commits \
                 (repository_id, commit_sha, pull_request_number, updated_at) \
                 VALUES (?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) \
                 ON CONFLICT(repository_id, commit_sha) DO UPDATE SET \
                 pull_request_number = excluded.pull_request_number, \
                 updated_at = excluded.updated_at",
            )
            .bind(repository_id.get())
            .bind(commit_sha.as_str())
            .bind(pull_request.get())
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(map_sqlx_error)
        })
        .await
    }

    /// Returns the pull request `commit_sha` was last seen on.
    ///
    /// # Errors
    ///
    /// Returns [`PullRequestCommitStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`PullRequestCommitStoreError::Internal`] for any other persistence failure, including a
    /// stored number that is no longer positive.
    pub(crate) async fn get(
        &self,
        repository_id: RepositoryId,
        commit_sha: &CommitSha,
    ) -> Result<Option<PullRequestNumber>, PullRequestCommitStoreError> {
        trace::instrument_database_operation(DatabaseOperation::PullRequestCommitGet, async {
            let number: Option<i64> = sqlx::query_scalar(
                "SELECT pull_request_number FROM pull_request_commits \
                 WHERE repository_id = ? AND commit_sha = ?",
            )
            .bind(repository_id.get())
            .bind(commit_sha.as_str())
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
            number
                .map(|number| {
                    PullRequestNumber::new(number)
                        .map_err(|_| PullRequestCommitStoreError::Internal)
                })
                .transpose()
        })
        .await
    }

    /// Deletes at most 1,000 commit mappings last updated before `cutoff`.
    ///
    /// # Errors
    ///
    /// Returns [`PullRequestCommitStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`PullRequestCommitStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn prune_batch(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<u64, PullRequestCommitStoreError> {
        trace::instrument_database_operation(DatabaseOperation::PullRequestCommitPrune, async {
            let cutoff = cutoff
                .to_offset(UtcOffset::UTC)
                .format(PULL_REQUEST_COMMIT_TIMESTAMP_FORMAT)
                .map_err(|_| PullRequestCommitStoreError::Internal)?;
            let result = sqlx::query(
                "DELETE FROM pull_request_commits WHERE rowid IN (\
                     SELECT rowid FROM pull_request_commits WHERE updated_at < ? \
                     ORDER BY updated_at, rowid LIMIT ?\
                 )",
            )
            .bind(cutoff)
            .bind(PULL_REQUEST_COMMIT_PRUNE_BATCH_SIZE)
            .execute(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
            Ok(result.rows_affected())
        })
        .await
    }
}

/// A stable, redacted pull-request commit persistence failure.
#[derive(Clone, Copy, Error, PartialEq, Eq)]
pub(crate) enum PullRequestCommitStoreError {
    /// SQLite is temporarily busy or locked.
    #[error("pull-request commit storage is temporarily unavailable")]
    Unavailable,
    /// SQLite returned an unexpected persistence failure whose details were discarded.
    #[error("internal pull-request commit persistence failure")]
    Internal,
}

impl fmt::Debug for PullRequestCommitStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "PullRequestCommitStoreError::Unavailable",
            Self::Internal => "PullRequestCommitStoreError::Internal",
        })
    }
}

fn map_sqlx_error(error: sqlx::Error) -> PullRequestCommitStoreError {
    if sqlite_is_busy_or_locked(&error) {
        PullRequestCommitStoreError::Unavailable
    } else {
        PullRequestCommitStoreError::Internal
    }
}

#[cfg(test)]
mod tests {
    use time::macros::datetime;

    use super::PullRequestCommitStore;
    use crate::{
        domain::{merge_queue::PullRequestNumber, repository::RepositoryId},
        storage::open_database,
        telemetry::trace::CommitSha,
    };

    #[tokio::test]
    async fn the_latest_pull_request_wins_and_prune_uses_update_time() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens");
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO repositories (full_name, webhook_secret_ciphertext, \
             webhook_secret_nonce, encryption_version, enabled, created_at, updated_at) \
             VALUES ('owner/repo', X'01', X'02', 1, 1, \
             '2026-11-18T10:00:00.000Z', '2026-11-18T10:00:00.000Z') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("repository inserts");
        let repository_id = RepositoryId::new(id).expect("repository id is positive");
        let store = PullRequestCommitStore::new(pool.clone());
        let sha = CommitSha::parse("0123456789ABCDEF0123456789abcdef01234567").expect("sha parses");
        let other =
            CommitSha::parse("1123456789abcdef0123456789abcdef01234567").expect("sha parses");

        for number in [7, 8] {
            store
                .record(
                    repository_id,
                    &sha,
                    PullRequestNumber::new(number).expect("PR is positive"),
                )
                .await
                .expect("mapping persists");
        }

        assert_eq!(
            store
                .get(repository_id, &sha)
                .await
                .expect("lookup succeeds")
                .map(PullRequestNumber::get),
            Some(8)
        );
        assert_eq!(
            store
                .get(repository_id, &other)
                .await
                .expect("lookup succeeds"),
            None
        );
        sqlx::query("UPDATE pull_request_commits SET updated_at = '2026-11-01T00:00:00.000Z'")
            .execute(&pool)
            .await
            .expect("fixture timestamp updates");
        assert_eq!(
            store
                .prune_batch(datetime!(2026-11-02 00:00:00 UTC))
                .await
                .expect("prune succeeds"),
            1
        );
    }
}
