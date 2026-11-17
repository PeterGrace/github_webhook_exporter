use std::fmt;

use sqlx::{SqliteConnection, SqlitePool};
use thiserror::Error;
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime, UtcOffset};

use crate::{
    domain::repository::RepositoryId,
    telemetry::{
        trace::{self, DatabaseOperation},
        usage::{UsageMeasure, UsageReport},
        workflow::{DisplayName, WorkflowRunAttempt, WorkflowRunId},
    },
};

use super::sqlite_is_busy_or_locked;

const CI_USAGE_PRUNE_BATCH_SIZE: i64 = 1_000;
const CI_USAGE_TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// Durable accepted in-job usage entries, keyed by repository, run and attempt for run totals.
#[derive(Clone)]
pub(crate) struct CiUsageStore {
    pool: SqlitePool,
}

impl CiUsageStore {
    /// Creates a usage store from an already migrated SQLite pool.
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Pins `repository_id` to the GitHub repository ID in its first usage token, and reports
    /// whether `github_repository_id` matches the pinned one.
    ///
    /// # Errors
    ///
    /// Returns [`CiUsageStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`CiUsageStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn pin_repository(
        &self,
        repository_id: RepositoryId,
        github_repository_id: i64,
    ) -> Result<bool, CiUsageStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::CiUsagePin,
            self.pin_repository_inner(repository_id, github_repository_id),
        )
        .await
    }

    async fn pin_repository_inner(
        &self,
        repository_id: RepositoryId,
        github_repository_id: i64,
    ) -> Result<bool, CiUsageStoreError> {
        sqlx::query(
            "INSERT INTO ci_usage_repository_pins (repository_id, github_repository_id, pinned_at) \
             VALUES (?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) \
             ON CONFLICT(repository_id) DO NOTHING",
        )
        .bind(repository_id.get())
        .bind(github_repository_id)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        let pinned: i64 = sqlx::query_scalar(
            "SELECT github_repository_id FROM ci_usage_repository_pins WHERE repository_id = ?",
        )
        .bind(repository_id.get())
        .fetch_one(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(pinned == github_repository_id)
    }

    /// Inserts every entry of one accepted post in a single transaction.
    ///
    /// # Errors
    ///
    /// Returns [`CiUsageStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`CiUsageStoreError::Internal`] for any other persistence failure. On error no entry of
    /// the post is stored.
    pub(crate) async fn record(
        &self,
        repository_id: RepositoryId,
        report: &UsageReport,
    ) -> Result<(), CiUsageStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::CiUsageRecord,
            self.record_inner(repository_id, report),
        )
        .await
    }

    async fn record_inner(
        &self,
        repository_id: RepositoryId,
        report: &UsageReport,
    ) -> Result<(), CiUsageStoreError> {
        let mut transaction = self.pool.begin().await.map_err(map_sqlx_error)?;
        for entry in &report.entries {
            let (input_tokens, output_tokens, cpu_seconds, ram_gb_seconds) = match entry.measure {
                UsageMeasure::Inference {
                    input_tokens,
                    output_tokens,
                } => (input_tokens, output_tokens, None, None),
                UsageMeasure::Sandbox {
                    cpu_seconds,
                    ram_gb_seconds,
                } => (None, None, Some(cpu_seconds), Some(ram_gb_seconds)),
            };
            sqlx::query(
                "INSERT INTO ci_usage_entries \
                 (repository_id, workflow_run_id, workflow_run_attempt, source, job, task, model, \
                  origin, cost_usd, price_fell_back, price_table_version, input_tokens, \
                  output_tokens, cpu_seconds, ram_gb_seconds, updated_at) \
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, \
                         strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
            )
            .bind(repository_id.get())
            .bind(report.claims.run_id.get())
            .bind(report.claims.run_attempt.get())
            .bind(entry.source.as_str())
            .bind(report.job.as_ref().map(DisplayName::as_str))
            .bind(entry.task.as_ref().map(DisplayName::as_str))
            .bind(entry.model.as_ref().map(DisplayName::as_str))
            .bind(entry.origin.as_ref().map(DisplayName::as_str))
            .bind(entry.cost_usd)
            .bind(entry.fell_back)
            .bind(entry.price_table_version.as_deref())
            .bind(input_tokens)
            .bind(output_tokens)
            .bind(cpu_seconds)
            .bind(ram_gb_seconds)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx_error)?;
        }
        transaction.commit().await.map_err(map_sqlx_error)
    }

    /// Sums the usage entries of one run attempt per source.
    ///
    /// # Errors
    ///
    /// Returns [`CiUsageStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`CiUsageStoreError::Internal`] for any other persistence failure.
    #[cfg(test)]
    pub(crate) async fn run_totals(
        &self,
        repository_id: RepositoryId,
        run_id: WorkflowRunId,
        run_attempt: WorkflowRunAttempt,
    ) -> Result<UsageTotals, CiUsageStoreError> {
        let mut connection = self.pool.acquire().await.map_err(map_sqlx_error)?;
        Self::run_totals_in(&mut connection, repository_id, run_id, run_attempt)
            .await
            .map_err(map_sqlx_error)
    }

    /// Runs the sums of [`Self::run_totals`] on `connection`, so a caller can read them inside its
    /// own transaction.
    pub(crate) async fn run_totals_in(
        connection: &mut SqliteConnection,
        repository_id: RepositoryId,
        run_id: WorkflowRunId,
        run_attempt: WorkflowRunAttempt,
    ) -> Result<UsageTotals, sqlx::Error> {
        let by_source: Vec<(String, f64, i64)> = sqlx::query_as(
            "SELECT source, TOTAL(cost_usd), COUNT(*) FROM ci_usage_entries \
             WHERE repository_id = ? AND workflow_run_id = ? AND workflow_run_attempt = ? \
             GROUP BY source",
        )
        .bind(repository_id.get())
        .bind(run_id.get())
        .bind(run_attempt.get())
        .fetch_all(&mut *connection)
        .await?;
        // Two versions are enough to tell that the attempt was priced by more than one table.
        let price_table_versions: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT price_table_version FROM ci_usage_entries \
             WHERE repository_id = ? AND workflow_run_id = ? AND workflow_run_attempt = ? \
             AND price_table_version IS NOT NULL LIMIT 2",
        )
        .bind(repository_id.get())
        .bind(run_id.get())
        .bind(run_attempt.get())
        .fetch_all(&mut *connection)
        .await?;
        Ok(UsageTotals {
            entries: by_source.iter().map(|(_, _, entries)| entries).sum(),
            cost_by_source: by_source
                .into_iter()
                .map(|(source, cost_usd, _)| (source, cost_usd))
                .collect(),
            price_table_versions,
        })
    }

    /// Deletes at most one bounded batch of entries last updated before `cutoff`.
    ///
    /// # Errors
    ///
    /// Returns [`CiUsageStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`CiUsageStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn prune_batch(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<u64, CiUsageStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::CiUsagePrune,
            self.prune_batch_inner(cutoff),
        )
        .await
    }

    async fn prune_batch_inner(&self, cutoff: OffsetDateTime) -> Result<u64, CiUsageStoreError> {
        let cutoff = cutoff
            .to_offset(UtcOffset::UTC)
            .format(CI_USAGE_TIMESTAMP_FORMAT)
            .map_err(|_| CiUsageStoreError::Internal)?;
        let result = sqlx::query(
            "DELETE FROM ci_usage_entries WHERE id IN (\
                 SELECT id FROM ci_usage_entries WHERE updated_at < ? \
                 ORDER BY updated_at, id LIMIT ?\
             )",
        )
        .bind(cutoff)
        .bind(CI_USAGE_PRUNE_BATCH_SIZE)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(result.rows_affected())
    }
}

/// The summed usage entries of one run attempt.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct UsageTotals {
    /// Summed USD per stored source.
    pub(crate) cost_by_source: Vec<(String, f64)>,
    /// The number of entries summed.
    pub(crate) entries: i64,
    /// At most two distinct price-table versions of the attempt's entries.
    pub(crate) price_table_versions: Vec<String>,
}

/// A stable, redacted usage persistence failure.
#[derive(Clone, Copy, Error, PartialEq, Eq)]
pub(crate) enum CiUsageStoreError {
    /// SQLite is temporarily busy or locked.
    #[error("usage storage is temporarily unavailable")]
    Unavailable,
    /// SQLite returned an unexpected persistence failure whose details were discarded.
    #[error("internal usage persistence failure")]
    Internal,
}

impl fmt::Debug for CiUsageStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "CiUsageStoreError::Unavailable",
            Self::Internal => "CiUsageStoreError::Internal",
        })
    }
}

fn map_sqlx_error(error: sqlx::Error) -> CiUsageStoreError {
    if sqlite_is_busy_or_locked(&error) {
        CiUsageStoreError::Unavailable
    } else {
        CiUsageStoreError::Internal
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use sqlx::{Row, SqlitePool};
    use time::macros::datetime;

    use super::CiUsageStore;
    use crate::{
        domain::repository::RepositoryId,
        security::{oidc::GitHubActionsClaims, CanonicalRepositoryName},
        storage::open_database,
        telemetry::{
            usage::{UsageEntry, UsageMeasure, UsageReport, UsageSource},
            workflow::{DisplayName, WorkflowEvent, WorkflowRunAttempt, WorkflowRunId},
        },
    };

    async fn store_with_repository() -> (tempfile::TempDir, SqlitePool, CiUsageStore, RepositoryId)
    {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens");
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO repositories (full_name, webhook_secret_ciphertext, \
             webhook_secret_nonce, encryption_version, enabled, created_at, updated_at) \
             VALUES ('owner/repo', X'01', X'02', 1, 1, \
             '2026-11-13T10:00:00.000Z', '2026-11-13T10:00:00.000Z') RETURNING id",
        )
        .fetch_one(&pool)
        .await
        .expect("repository inserts");
        let store = CiUsageStore::new(pool.clone());
        (
            directory,
            pool,
            store,
            RepositoryId::new(id).expect("repository id is positive"),
        )
    }

    fn report(entries: Vec<UsageEntry>) -> UsageReport {
        UsageReport {
            claims: GitHubActionsClaims {
                repository: CanonicalRepositoryName::new("owner/repo").expect("name is valid"),
                github_repository_id: 4242,
                workflow: DisplayName::sanitize("CI"),
                workflow_file: None,
                job_workflow_file: None,
                run_id: WorkflowRunId::new(31).expect("run id is positive"),
                run_attempt: WorkflowRunAttempt::new(2).expect("attempt is positive"),
                event: WorkflowEvent::PullRequest,
                sha: None,
            },
            job: DisplayName::sanitize("run-evals"),
            pull_request: None,
            received_at: SystemTime::now(),
            entries,
        }
    }

    fn inference(usd: f64) -> UsageEntry {
        UsageEntry {
            source: UsageSource::parse("anthropic").expect("source is valid"),
            task: DisplayName::sanitize("grade"),
            model: DisplayName::sanitize("claude-sonnet-5-5"),
            origin: None,
            cost_usd: usd,
            fell_back: false,
            price_table_version: None,
            measure: UsageMeasure::Inference {
                input_tokens: Some(88_400),
                output_tokens: Some(9_100),
            },
        }
    }

    fn sandbox() -> UsageEntry {
        UsageEntry {
            source: UsageSource::parse("daytona").expect("source is valid"),
            task: None,
            model: None,
            origin: None,
            cost_usd: 0.0,
            fell_back: true,
            price_table_version: Some("0123456789ab".to_owned()),
            measure: UsageMeasure::Sandbox {
                cpu_seconds: 1_840.0,
                ram_gb_seconds: 7_360.0,
            },
        }
    }

    #[tokio::test]
    async fn every_entry_of_a_post_is_stored_with_its_run() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;

        store
            .record(repository_id, &report(vec![inference(0.381), sandbox()]))
            .await
            .expect("entries persist");

        let rows = sqlx::query(
            "SELECT workflow_run_id, workflow_run_attempt, source, job, task, model, cost_usd, \
             price_fell_back, price_table_version, input_tokens, cpu_seconds \
             FROM ci_usage_entries ORDER BY id",
        )
        .fetch_all(&pool)
        .await
        .expect("rows load");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].get::<i64, _>("workflow_run_id"), 31);
        assert_eq!(rows[0].get::<i64, _>("workflow_run_attempt"), 2);
        assert_eq!(rows[0].get::<String, _>("source"), "anthropic");
        assert_eq!(
            rows[0].get::<Option<String>, _>("job").as_deref(),
            Some("run-evals")
        );
        assert_eq!(
            rows[0].get::<Option<String>, _>("task").as_deref(),
            Some("grade")
        );
        assert_eq!(rows[0].get::<f64, _>("cost_usd"), 0.381);
        assert_eq!(rows[0].get::<Option<i64>, _>("input_tokens"), Some(88_400));
        assert_eq!(rows[0].get::<Option<f64>, _>("cpu_seconds"), None);
        assert_eq!(rows[1].get::<String, _>("source"), "daytona");
        assert!(rows[1].get::<bool, _>("price_fell_back"));
        assert_eq!(
            rows[1]
                .get::<Option<String>, _>("price_table_version")
                .as_deref(),
            Some("0123456789ab")
        );
        assert_eq!(rows[1].get::<Option<f64>, _>("cpu_seconds"), Some(1_840.0));
    }

    #[tokio::test]
    async fn a_rejected_entry_stores_nothing_from_its_post() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;

        store
            .record(
                repository_id,
                &report(vec![inference(0.5), inference(-1.0)]),
            )
            .await
            .expect_err("negative cost violates the schema");

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ci_usage_entries")
            .fetch_one(&pool)
            .await
            .expect("count loads");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn the_table_rejects_values_the_handler_would_refuse() {
        let (_directory, pool, _store, repository_id) = store_with_repository().await;
        for (source, cpu_seconds) in [("Anthropic", None), ("a b", None), ("daytona", Some(2e9))] {
            let inserted = sqlx::query(
                "INSERT INTO ci_usage_entries (repository_id, workflow_run_id, \
                 workflow_run_attempt, source, cost_usd, price_fell_back, cpu_seconds, \
                 ram_gb_seconds, updated_at) \
                 VALUES (?, 31, 1, ?, 0, 0, ?, ?, '2026-11-13T10:00:00.000Z')",
            )
            .bind(repository_id.get())
            .bind(source)
            .bind(cpu_seconds)
            .bind(cpu_seconds.map(|_| 1.0))
            .execute(&pool)
            .await;
            assert!(inserted.is_err(), "{source} {cpu_seconds:?}");
        }
    }

    #[tokio::test]
    async fn the_first_repository_id_seen_is_pinned() {
        let (_directory, _pool, store, repository_id) = store_with_repository().await;

        assert!(store
            .pin_repository(repository_id, 4242)
            .await
            .expect("pins"));
        assert!(store
            .pin_repository(repository_id, 4242)
            .await
            .expect("matches"));
        assert!(!store
            .pin_repository(repository_id, 9999)
            .await
            .expect("lookup succeeds"));
    }

    #[tokio::test]
    async fn prune_removes_only_entries_older_than_the_cutoff() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;
        store
            .record(repository_id, &report(vec![inference(0.1), inference(0.2)]))
            .await
            .expect("entries persist");
        sqlx::query(
            "UPDATE ci_usage_entries SET updated_at = '2026-01-01T00:00:00.000Z' WHERE id = 1",
        )
        .execute(&pool)
        .await
        .expect("row ages");

        let pruned = store
            .prune_batch(datetime!(2026-06-01 00:00 UTC))
            .await
            .expect("prune succeeds");

        assert_eq!(pruned, 1);
        let remaining: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ci_usage_entries")
            .fetch_one(&pool)
            .await
            .expect("count loads");
        assert_eq!(remaining, 1);
    }

    #[tokio::test]
    async fn run_totals_sum_one_attempt_per_source() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;
        store
            .record(
                repository_id,
                &report(vec![inference(0.25), inference(0.5), sandbox()]),
            )
            .await
            .expect("entries persist");
        let mut other_attempt = report(vec![inference(9.0)]);
        other_attempt.claims.run_attempt = WorkflowRunAttempt::new(1).expect("attempt is positive");
        store
            .record(repository_id, &other_attempt)
            .await
            .expect("other attempt persists");
        sqlx::query(
            "INSERT INTO repositories (full_name, webhook_secret_ciphertext, \
             webhook_secret_nonce, encryption_version, enabled, created_at, updated_at) \
             VALUES ('owner/other', X'01', X'02', 1, 1, \
             '2026-11-13T10:00:00.000Z', '2026-11-13T10:00:00.000Z')",
        )
        .execute(&pool)
        .await
        .expect("other repository inserts");
        let other_repository = RepositoryId::new(repository_id.get() + 1).expect("id is positive");
        store
            .record(other_repository, &report(vec![inference(7.0)]))
            .await
            .expect("other repository persists");

        let mut totals = store
            .run_totals(
                repository_id,
                WorkflowRunId::new(31).expect("run id is positive"),
                WorkflowRunAttempt::new(2).expect("attempt is positive"),
            )
            .await
            .expect("totals load");
        totals
            .cost_by_source
            .sort_by(|left, right| left.0.cmp(&right.0));

        assert_eq!(totals.entries, 3);
        assert_eq!(
            totals.cost_by_source,
            [("anthropic".to_owned(), 0.75), ("daytona".to_owned(), 0.0)]
        );
        assert_eq!(totals.price_table_versions, ["0123456789ab"]);
    }
}
