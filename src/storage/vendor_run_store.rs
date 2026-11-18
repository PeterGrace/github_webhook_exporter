use std::fmt;

use sqlx::SqlitePool;
use thiserror::Error;
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime, UtcOffset};

use crate::{
    domain::repository::RepositoryId,
    telemetry::{
        trace::{self, CommitSha, DatabaseOperation},
        vendor::VendorRun,
    },
};

use super::sqlite_is_busy_or_locked;

const VENDOR_RUN_PRUNE_BATCH_SIZE: i64 = 1_000;
const VENDOR_RUN_TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// Durable priced vendor runs, keyed by repository and pull request for per-PR totals.
#[derive(Clone)]
pub(crate) struct VendorRunStore {
    pool: SqlitePool,
}

impl VendorRunStore {
    /// Creates a vendor-run store from an already migrated SQLite pool.
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Stores one vendor run, or nothing when it is not attributed to a single pull request.
    ///
    /// # Errors
    ///
    /// Returns [`VendorRunStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`VendorRunStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn record(
        &self,
        repository_id: RepositoryId,
        run: &VendorRun,
    ) -> Result<(), VendorRunStoreError> {
        let Some(pull_request) = run.pull_request else {
            return Ok(());
        };
        trace::instrument_database_operation(DatabaseOperation::VendorRunRecord, async {
            sqlx::query(
                "INSERT INTO ci_vendor_runs \
                 (repository_id, pull_request_number, head_sha, vendor, trigger, cost_usd, \
                  price_table_version, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
            )
            .bind(repository_id.get())
            .bind(pull_request.get())
            .bind(run.head_sha.as_ref().map(CommitSha::as_str))
            .bind(run.vendor.as_str())
            .bind(run.trigger.as_str())
            .bind(run.cost_usd)
            .bind(run.price_table_version.as_str())
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(map_sqlx_error)
        })
        .await
    }

    /// Deletes at most one bounded batch of runs last updated before `cutoff`.
    ///
    /// # Errors
    ///
    /// Returns [`VendorRunStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`VendorRunStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn prune_batch(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<u64, VendorRunStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::VendorRunPrune,
            self.prune_batch_inner(cutoff),
        )
        .await
    }

    async fn prune_batch_inner(&self, cutoff: OffsetDateTime) -> Result<u64, VendorRunStoreError> {
        let cutoff = cutoff
            .to_offset(UtcOffset::UTC)
            .format(VENDOR_RUN_TIMESTAMP_FORMAT)
            .map_err(|_| VendorRunStoreError::Internal)?;
        let result = sqlx::query(
            "DELETE FROM ci_vendor_runs WHERE id IN (\
                 SELECT id FROM ci_vendor_runs WHERE updated_at < ? \
                 ORDER BY updated_at, id LIMIT ?\
             )",
        )
        .bind(cutoff)
        .bind(VENDOR_RUN_PRUNE_BATCH_SIZE)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(result.rows_affected())
    }
}

/// A stable, redacted vendor-run persistence failure.
#[derive(Clone, Copy, Error, PartialEq, Eq)]
pub(crate) enum VendorRunStoreError {
    /// SQLite is temporarily busy or locked.
    #[error("vendor-run storage is temporarily unavailable")]
    Unavailable,
    /// SQLite returned an unexpected persistence failure whose details were discarded.
    #[error("internal vendor-run persistence failure")]
    Internal,
}

impl fmt::Debug for VendorRunStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "VendorRunStoreError::Unavailable",
            Self::Internal => "VendorRunStoreError::Internal",
        })
    }
}

fn map_sqlx_error(error: sqlx::Error) -> VendorRunStoreError {
    if sqlite_is_busy_or_locked(&error) {
        VendorRunStoreError::Unavailable
    } else {
        VendorRunStoreError::Internal
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use sqlx::{Row, SqlitePool};
    use time::macros::datetime;

    use super::VendorRunStore;
    use crate::{
        domain::{merge_queue::PullRequestNumber, repository::RepositoryId},
        security::CanonicalRepositoryName,
        storage::open_database,
        telemetry::{
            trace::CommitSha,
            vendor::{VendorRun, VendorTrigger},
            workflow::DisplayName,
        },
    };

    const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    async fn store_with_repository() -> (tempfile::TempDir, SqlitePool, VendorRunStore, RepositoryId)
    {
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
        let store = VendorRunStore::new(pool.clone());
        (
            directory,
            pool,
            store,
            RepositoryId::new(id).expect("repository id is positive"),
        )
    }

    fn run(pull_request: Option<i64>, cost_usd: f64) -> VendorRun {
        VendorRun {
            repository_name: CanonicalRepositoryName::new("owner/repo").expect("name is valid"),
            vendor: DisplayName::sanitize("example-review-bot").expect("name is valid"),
            trigger: VendorTrigger::PullRequestReview,
            cost_usd,
            price_table_version: "0123456789ab".to_owned(),
            pull_request: pull_request
                .map(|number| PullRequestNumber::new(number).expect("number is positive")),
            head_sha: Some(CommitSha::parse(HEAD_SHA).expect("sha is valid")),
            received_at: SystemTime::now(),
        }
    }

    async fn row_count(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM ci_vendor_runs")
            .fetch_one(pool)
            .await
            .expect("count loads")
    }

    #[tokio::test]
    async fn an_attributed_run_is_stored_with_its_pull_request() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;

        store
            .record(repository_id, &run(Some(42), 0.5))
            .await
            .expect("run persists");

        let row = sqlx::query(
            "SELECT repository_id, pull_request_number, head_sha, vendor, trigger, cost_usd, \
             price_table_version FROM ci_vendor_runs",
        )
        .fetch_one(&pool)
        .await
        .expect("row loads");
        assert_eq!(row.get::<i64, _>("repository_id"), repository_id.get());
        assert_eq!(row.get::<i64, _>("pull_request_number"), 42);
        assert_eq!(
            row.get::<Option<String>, _>("head_sha").as_deref(),
            Some(HEAD_SHA)
        );
        assert_eq!(row.get::<String, _>("vendor"), "example-review-bot");
        assert_eq!(row.get::<String, _>("trigger"), "pull_request_review");
        assert_eq!(row.get::<f64, _>("cost_usd"), 0.5);
        assert_eq!(row.get::<String, _>("price_table_version"), "0123456789ab");
    }

    #[tokio::test]
    async fn an_unattributed_run_stores_nothing() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;

        store
            .record(repository_id, &run(None, 0.5))
            .await
            .expect("nothing to store is not an error");

        assert_eq!(row_count(&pool).await, 0);
    }

    #[tokio::test]
    async fn a_negative_cost_is_rejected_as_internal() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;

        let failure = store
            .record(repository_id, &run(Some(42), -1.0))
            .await
            .expect_err("negative cost violates the schema");

        assert_eq!(failure, super::VendorRunStoreError::Internal);
        assert_eq!(row_count(&pool).await, 0);
    }

    #[tokio::test]
    async fn the_table_rejects_values_the_handler_would_refuse() {
        let (_directory, pool, _store, repository_id) = store_with_repository().await;
        let long_vendor = "v".repeat(129);
        let upper_sha = HEAD_SHA.to_uppercase();
        let cases: [(i64, Option<&str>, &str, &str, &str); 7] = [
            (0, None, "v", "check_suite", "0123456789ab"),
            (1, Some("abc"), "v", "check_suite", "0123456789ab"),
            (
                1,
                Some(upper_sha.as_str()),
                "v",
                "check_suite",
                "0123456789ab",
            ),
            (1, None, "", "check_suite", "0123456789ab"),
            (1, None, long_vendor.as_str(), "check_suite", "0123456789ab"),
            (1, None, "v", "check_run", "0123456789ab"),
            (1, None, "v", "check_suite", "short"),
        ];
        for (number, head_sha, vendor, trigger, version) in cases {
            let inserted = sqlx::query(
                "INSERT INTO ci_vendor_runs (repository_id, pull_request_number, head_sha, \
                 vendor, trigger, cost_usd, price_table_version, updated_at) \
                 VALUES (?, ?, ?, ?, ?, 0, ?, '2026-11-18T10:00:00.000Z')",
            )
            .bind(repository_id.get())
            .bind(number)
            .bind(head_sha)
            .bind(vendor)
            .bind(trigger)
            .bind(version)
            .execute(&pool)
            .await;
            assert!(
                inserted.is_err(),
                "{number} {head_sha:?} {vendor} {trigger} {version}"
            );
        }
    }

    #[tokio::test]
    async fn prune_removes_only_runs_older_than_the_cutoff() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;
        for _ in 0..2 {
            store
                .record(repository_id, &run(Some(42), 0.5))
                .await
                .expect("run persists");
        }
        sqlx::query(
            "UPDATE ci_vendor_runs SET updated_at = '2026-01-01T00:00:00.000Z' WHERE id = 1",
        )
        .execute(&pool)
        .await
        .expect("row ages");

        let pruned = store
            .prune_batch(datetime!(2026-06-01 00:00 UTC))
            .await
            .expect("prune succeeds");

        assert_eq!(pruned, 1);
        assert_eq!(row_count(&pool).await, 1);
    }
}
