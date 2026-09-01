use std::{fmt, time::Duration};

use sqlx::{FromRow, SqlitePool};
use thiserror::Error;
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime, UtcOffset};

use crate::{
    domain::repository::RepositoryId,
    telemetry::{
        trace::{self, DatabaseOperation},
        workflow::{DisplayName, RequiredChecks, WorkflowBranch, MAX_REQUIRED_CHECK_COUNT},
    },
};

use super::sqlite_is_busy_or_locked;

const REQUIRED_CHECK_PRUNE_BATCH_SIZE: i64 = 1_000;
const REQUIRED_CHECK_TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");
/// Separates encoded check names in the stored column.
///
/// [`DisplayName::sanitize`] removes every Unicode control character, so a newline can never occur
/// inside a name. That makes newline-joining a lossless, injection-free encoding of the set.
const CHECK_NAME_SEPARATOR: char = '\n';

/// A cache of branch-protection required status checks, keyed by repository branch.
///
/// The synchronous webhook path reads this store and never calls GitHub itself. Entries older than
/// the configured time-to-live are reported as absent, so a stale answer is never emitted as a
/// confident one; refreshing them is the out-of-band refresher's job.
#[derive(Clone)]
pub(crate) struct RequiredCheckStore {
    pool: SqlitePool,
    time_to_live: Duration,
}

impl RequiredCheckStore {
    /// Creates a required-check cache from an already migrated SQLite pool.
    ///
    /// # Parameters
    ///
    /// * `pool` - The migrated SQLite pool shared by every store.
    /// * `time_to_live` - How long a cached entry stays confident after it was written.
    pub(crate) fn new(pool: SqlitePool, time_to_live: Duration) -> Self {
        Self { pool, time_to_live }
    }

    /// Inserts or replaces the required-check set cached for one repository branch.
    ///
    /// # Errors
    ///
    /// Returns [`RequiredCheckStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`RequiredCheckStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn upsert(
        &self,
        repository_id: RepositoryId,
        branch: &WorkflowBranch,
        checks: &RequiredChecks,
    ) -> Result<(), RequiredCheckStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::RequiredCheckUpsert,
            self.upsert_inner(repository_id, branch, checks),
        )
        .await
    }

    async fn upsert_inner(
        &self,
        repository_id: RepositoryId,
        branch: &WorkflowBranch,
        checks: &RequiredChecks,
    ) -> Result<(), RequiredCheckStoreError> {
        sqlx::query(
            "INSERT INTO required_check_contexts \
             (repository_id, target_branch, check_names, updated_at) \
             VALUES (?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) \
             ON CONFLICT(repository_id, target_branch) DO UPDATE SET \
             check_names = excluded.check_names, updated_at = excluded.updated_at",
        )
        .bind(repository_id.get())
        .bind(branch.as_str())
        .bind(encode_check_names(checks))
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(())
    }

    /// Loads the required-check set cached for one repository branch, when it is still fresh.
    ///
    /// # Parameters
    ///
    /// * `repository_id` - The authenticated repository.
    /// * `branch` - The workflow run's target branch.
    /// * `now` - The instant the freshness window is measured against.
    ///
    /// # Returns
    ///
    /// `Some` set when an entry exists and is younger than the configured time-to-live, and `None`
    /// when the entry is absent or stale. `None` is the caller's signal to emit "unknown" and to
    /// ask the out-of-band refresher for a new answer.
    ///
    /// # Errors
    ///
    /// Returns [`RequiredCheckStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`RequiredCheckStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn get_fresh(
        &self,
        repository_id: RepositoryId,
        branch: &WorkflowBranch,
        now: OffsetDateTime,
    ) -> Result<Option<RequiredChecks>, RequiredCheckStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::RequiredCheckGet,
            self.get_fresh_inner(repository_id, branch, now),
        )
        .await
    }

    async fn get_fresh_inner(
        &self,
        repository_id: RepositoryId,
        branch: &WorkflowBranch,
        now: OffsetDateTime,
    ) -> Result<Option<RequiredChecks>, RequiredCheckStoreError> {
        let freshness_cutoff = format_timestamp(
            now.checked_sub(
                time::Duration::try_from(self.time_to_live)
                    .map_err(|_| RequiredCheckStoreError::Internal)?,
            )
            .ok_or(RequiredCheckStoreError::Internal)?,
        )?;
        let row = sqlx::query_as::<_, StoredRequiredChecks>(
            "SELECT check_names FROM required_check_contexts \
             WHERE repository_id = ? AND target_branch = ? AND updated_at >= ?",
        )
        .bind(repository_id.get())
        .bind(branch.as_str())
        .bind(freshness_cutoff)
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        Ok(row.map(StoredRequiredChecks::into_checks))
    }

    /// Deletes at most 1,000 cached required-check rows last updated before `cutoff`.
    ///
    /// # Errors
    ///
    /// Returns [`RequiredCheckStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`RequiredCheckStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn prune_batch(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<u64, RequiredCheckStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::RequiredCheckPrune,
            self.prune_batch_inner(cutoff),
        )
        .await
    }

    async fn prune_batch_inner(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<u64, RequiredCheckStoreError> {
        let cutoff = format_timestamp(cutoff)?;
        let result = sqlx::query(
            "DELETE FROM required_check_contexts WHERE rowid IN (\
                 SELECT rowid FROM required_check_contexts WHERE updated_at < ? \
                 ORDER BY updated_at, rowid LIMIT ?\
             )",
        )
        .bind(cutoff)
        .bind(REQUIRED_CHECK_PRUNE_BATCH_SIZE)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(result.rows_affected())
    }
}

fn format_timestamp(value: OffsetDateTime) -> Result<String, RequiredCheckStoreError> {
    value
        .to_offset(UtcOffset::UTC)
        .format(REQUIRED_CHECK_TIMESTAMP_FORMAT)
        .map_err(|_| RequiredCheckStoreError::Internal)
}

fn encode_check_names(checks: &RequiredChecks) -> String {
    checks
        .names()
        .collect::<Vec<_>>()
        .join(&CHECK_NAME_SEPARATOR.to_string())
}

#[derive(FromRow)]
struct StoredRequiredChecks {
    check_names: String,
}

impl StoredRequiredChecks {
    /// Decodes the stored column back into a bounded set.
    ///
    /// Names are re-sanitized rather than trusted: a row written by an older build, or edited out
    /// of band, cannot smuggle an unbounded or control-bearing name into a span attribute.
    fn into_checks(self) -> RequiredChecks {
        RequiredChecks::new(
            self.check_names
                .split(CHECK_NAME_SEPARATOR)
                .filter_map(DisplayName::sanitize)
                .take(MAX_REQUIRED_CHECK_COUNT),
        )
    }
}

/// A stable, redacted required-check cache persistence failure.
#[derive(Clone, Copy, Error, PartialEq, Eq)]
pub(crate) enum RequiredCheckStoreError {
    /// SQLite is temporarily busy or locked.
    #[error("required-check storage is temporarily unavailable")]
    Unavailable,
    /// SQLite returned an unexpected persistence failure whose details were discarded.
    #[error("internal required-check persistence failure")]
    Internal,
}

impl fmt::Debug for RequiredCheckStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "RequiredCheckStoreError::Unavailable",
            Self::Internal => "RequiredCheckStoreError::Internal",
        })
    }
}

fn map_sqlx_error(error: sqlx::Error) -> RequiredCheckStoreError {
    if sqlite_is_busy_or_locked(&error) {
        RequiredCheckStoreError::Unavailable
    } else {
        RequiredCheckStoreError::Internal
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sqlx::{Row, SqlitePool};
    use time::OffsetDateTime;

    use super::RequiredCheckStore;
    use crate::{
        domain::repository::RepositoryId,
        storage::open_database,
        telemetry::workflow::{DisplayName, RequiredChecks, WorkflowBranch},
    };

    const TTL: Duration = Duration::from_secs(300);

    fn branch(value: &str) -> WorkflowBranch {
        WorkflowBranch::sanitize(value).expect("branch name is valid")
    }

    fn checks(names: &[&str]) -> RequiredChecks {
        RequiredChecks::new(names.iter().filter_map(|name| DisplayName::sanitize(name)))
    }

    async fn insert_repository(pool: &SqlitePool) -> RepositoryId {
        let id = sqlx::query_scalar(
            "INSERT INTO repositories (full_name, webhook_secret_ciphertext, \
             webhook_secret_nonce, encryption_version, enabled, created_at, updated_at) \
             VALUES ('owner/repo', X'01', X'02', 1, 1, \
             '2026-09-01T10:00:00.000Z', '2026-09-01T10:00:00.000Z') RETURNING id",
        )
        .fetch_one(pool)
        .await
        .expect("repository inserts");
        RepositoryId::new(id).expect("repository id is positive")
    }

    #[tokio::test]
    async fn migration_contains_only_bounded_cache_fields() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens");

        let columns = sqlx::query("PRAGMA table_info(required_check_contexts)")
            .fetch_all(&pool)
            .await
            .expect("schema is inspectable")
            .into_iter()
            .map(|row| row.get::<String, _>("name"))
            .collect::<Vec<_>>();

        assert_eq!(
            columns,
            [
                "repository_id",
                "target_branch",
                "check_names",
                "updated_at",
            ]
        );
        for forbidden in ["token", "installation_id", "private_key", "payload"] {
            assert!(!columns.iter().any(|column| column == forbidden));
        }
    }

    #[tokio::test]
    async fn a_fresh_entry_round_trips_and_keeps_branches_independent() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens");
        let repository_id = insert_repository(&pool).await;
        let store = RequiredCheckStore::new(pool, TTL);

        store
            .upsert(repository_id, &branch("main"), &checks(&["build", "test"]))
            .await
            .expect("main entry persists");
        store
            .upsert(repository_id, &branch("release"), &checks(&["ship"]))
            .await
            .expect("release entry persists");

        let now = OffsetDateTime::now_utc();
        let main = store
            .get_fresh(repository_id, &branch("main"), now)
            .await
            .expect("main entry reads")
            .expect("main entry is fresh");
        let release = store
            .get_fresh(repository_id, &branch("release"), now)
            .await
            .expect("release entry reads")
            .expect("release entry is fresh");

        assert_eq!(main.names().collect::<Vec<_>>(), ["build", "test"]);
        assert_eq!(release.names().collect::<Vec<_>>(), ["ship"]);
    }

    #[tokio::test]
    async fn an_empty_set_is_a_confident_answer_rather_than_a_miss() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens");
        let repository_id = insert_repository(&pool).await;
        let store = RequiredCheckStore::new(pool, TTL);

        store
            .upsert(repository_id, &branch("main"), &checks(&[]))
            .await
            .expect("empty entry persists");

        let cached = store
            .get_fresh(repository_id, &branch("main"), OffsetDateTime::now_utc())
            .await
            .expect("entry reads")
            .expect("an unprotected branch still caches an answer");

        assert_eq!(cached.len(), 0);
    }

    #[tokio::test]
    async fn an_entry_older_than_the_time_to_live_reads_as_absent() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens");
        let repository_id = insert_repository(&pool).await;
        let store = RequiredCheckStore::new(pool, TTL);
        store
            .upsert(repository_id, &branch("main"), &checks(&["build"]))
            .await
            .expect("entry persists");

        let now = OffsetDateTime::now_utc();
        assert!(store
            .get_fresh(repository_id, &branch("main"), now)
            .await
            .expect("fresh read succeeds")
            .is_some());
        assert!(store
            .get_fresh(
                repository_id,
                &branch("main"),
                now + time::Duration::seconds(301)
            )
            .await
            .expect("stale read succeeds")
            .is_none());
    }

    #[tokio::test]
    async fn an_upsert_replaces_the_cached_set_and_refreshes_its_age() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens");
        let repository_id = insert_repository(&pool).await;
        let store = RequiredCheckStore::new(pool.clone(), TTL);

        store
            .upsert(repository_id, &branch("main"), &checks(&["build"]))
            .await
            .expect("first entry persists");
        sqlx::query("UPDATE required_check_contexts SET updated_at = '2020-01-01T00:00:00.000Z'")
            .execute(&pool)
            .await
            .expect("entry ages");
        store
            .upsert(repository_id, &branch("main"), &checks(&["ship"]))
            .await
            .expect("second entry persists");

        let cached = store
            .get_fresh(repository_id, &branch("main"), OffsetDateTime::now_utc())
            .await
            .expect("entry reads")
            .expect("the replaced entry is fresh again");

        assert_eq!(cached.names().collect::<Vec<_>>(), ["ship"]);
    }

    #[tokio::test]
    async fn pruning_deletes_only_entries_older_than_the_cutoff() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens");
        let repository_id = insert_repository(&pool).await;
        let store = RequiredCheckStore::new(pool.clone(), TTL);
        store
            .upsert(repository_id, &branch("stale"), &checks(&["build"]))
            .await
            .expect("stale entry persists");
        sqlx::query(
            "UPDATE required_check_contexts SET updated_at = '2020-01-01T00:00:00.000Z' \
             WHERE target_branch = 'stale'",
        )
        .execute(&pool)
        .await
        .expect("entry ages");
        store
            .upsert(repository_id, &branch("current"), &checks(&["test"]))
            .await
            .expect("current entry persists");

        // A cutoff one minute in the past is safely after the aged row and before the fresh one.
        let deleted = store
            .prune_batch(OffsetDateTime::now_utc() - time::Duration::minutes(1))
            .await
            .expect("prune succeeds");

        assert_eq!(deleted, 1);
        let remaining: Vec<String> =
            sqlx::query_scalar("SELECT target_branch FROM required_check_contexts")
                .fetch_all(&pool)
                .await
                .expect("remaining rows read");
        assert_eq!(remaining, ["current"]);
    }

    #[tokio::test]
    async fn stored_names_are_resanitized_when_they_are_read_back() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens");
        let repository_id = insert_repository(&pool).await;
        let store = RequiredCheckStore::new(pool.clone(), TTL);
        store
            .upsert(repository_id, &branch("main"), &checks(&["build"]))
            .await
            .expect("entry persists");
        // Simulate a row written out of band that carries control characters and an empty entry.
        sqlx::query("UPDATE required_check_contexts SET check_names = ?")
            .bind("bui\u{0007}ld\n\nship")
            .execute(&pool)
            .await
            .expect("row is rewritten");

        let cached = store
            .get_fresh(repository_id, &branch("main"), OffsetDateTime::now_utc())
            .await
            .expect("entry reads")
            .expect("entry is fresh");

        assert_eq!(cached.names().collect::<Vec<_>>(), ["build", "ship"]);
    }
}
