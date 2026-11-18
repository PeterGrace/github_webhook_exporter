use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use opentelemetry::trace::{SpanId, TraceId};
use sqlx::{FromRow, SqliteConnection, SqlitePool};
use thiserror::Error;
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime, UtcOffset};

use crate::{
    domain::repository::RepositoryId,
    telemetry::{
        pipeline::{PipelineJobSummary, WorkflowJobTraceIdentity},
        run_cost::{
            breakdown_key, DEFAULT_RUNNER_LABEL_KEY, UNLABELLED_RUNNER_LABEL_KEY,
            UNPRICED_RUNNER_LABEL_KEY,
        },
        trace::{self, DatabaseOperation},
        workflow::{
            DisplayName, HistoricalTiming, TimingSource, WorkflowConclusion, WorkflowJobId,
            WorkflowJobTrace, WorkflowRunAttempt, WorkflowRunId,
        },
    },
};

use super::{run_cost_store::attempt_is_final, sqlite_is_busy_or_locked};

const WORKFLOW_JOB_LINK_PRUNE_BATCH_SIZE: i64 = 1_000;
const WORKFLOW_JOB_LINK_TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// Durable identities and billing of completed workflow jobs, used to link pipeline-run
/// summaries and to total run costs.
///
/// Only what a pipeline-run summary span and a run total need is retained: the exported trace and
/// span identifiers when a job trace was exported, the sanitized job name, the bounded conclusion,
/// the historical interval that the job trace itself used, and the job's minutes, price and
/// normalized price-table label.
#[derive(Clone)]
pub(crate) struct WorkflowJobLinkStore {
    pool: SqlitePool,
}

impl WorkflowJobLinkStore {
    /// Creates a workflow-job link store from an already migrated SQLite pool.
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Inserts or replaces one completed job's link and billing.
    ///
    /// # Parameters
    ///
    /// * `repository_id` - The authenticated repository row identifier.
    /// * `job` - The bounded completed job.
    /// * `identity` - The exported identity of the job trace's root span, or `None` when no job
    ///   trace was exported; such a row counts toward run totals but never links a pipeline span.
    ///
    /// # Returns
    ///
    /// Whether the run attempt was already final when the write committed. The check runs in the
    /// write's own transaction, under SQLite's write lock, so `true` means the record is not in
    /// the attempt's total and `false` means it will be.
    ///
    /// # Errors
    ///
    /// Returns [`WorkflowJobLinkStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`WorkflowJobLinkStoreError::Internal`] for any other persistence failure, including a
    /// timing interval that cannot be represented as nanoseconds since the Unix epoch.
    pub(crate) async fn record(
        &self,
        repository_id: RepositoryId,
        job: &WorkflowJobTrace,
        identity: Option<WorkflowJobTraceIdentity>,
    ) -> Result<bool, WorkflowJobLinkStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::WorkflowJobLinkRecord,
            self.record_inner(repository_id, job, identity),
        )
        .await
    }

    async fn record_inner(
        &self,
        repository_id: RepositoryId,
        job: &WorkflowJobTrace,
        identity: Option<WorkflowJobTraceIdentity>,
    ) -> Result<bool, WorkflowJobLinkStoreError> {
        if identity.is_some_and(|identity| !identity.is_valid()) {
            return Err(WorkflowJobLinkStoreError::Internal);
        }
        let started_at_nanos = system_time_to_nanos(job.timing().start())
            .ok_or(WorkflowJobLinkStoreError::Internal)?;
        let completed_at_nanos =
            system_time_to_nanos(job.timing().end()).ok_or(WorkflowJobLinkStoreError::Internal)?;
        let billing = job.billing();
        let cost = billing.and_then(|billing| billing.cost.as_ref());
        let runner_label = cost.map(|cost| {
            cost.price_label
                .as_deref()
                .and_then(breakdown_key)
                .unwrap_or_else(|| DEFAULT_RUNNER_LABEL_KEY.to_owned())
        });

        let mut transaction = self
            .pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(map_sqlx_error)?;
        sqlx::query(
            "INSERT INTO workflow_job_links \
             (repository_id, workflow_run_id, workflow_run_attempt, workflow_job_id, job_name, \
              conclusion, trace_id, span_id, started_at_nanos, completed_at_nanos, timing_source, \
              billable_minutes, cost_usd, price_table_version, runner_label, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, \
                     strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) \
             ON CONFLICT(repository_id, workflow_run_id, workflow_run_attempt, workflow_job_id) \
             DO UPDATE SET job_name = excluded.job_name, conclusion = excluded.conclusion, \
             trace_id = excluded.trace_id, span_id = excluded.span_id, \
             started_at_nanos = excluded.started_at_nanos, \
             completed_at_nanos = excluded.completed_at_nanos, \
             timing_source = excluded.timing_source, \
             billable_minutes = excluded.billable_minutes, cost_usd = excluded.cost_usd, \
             price_table_version = excluded.price_table_version, \
             runner_label = excluded.runner_label, updated_at = excluded.updated_at",
        )
        .bind(repository_id.get())
        .bind(job.run_id().get())
        .bind(job.run_attempt().get())
        .bind(job.job_id().get())
        .bind(job.job_name().map(DisplayName::as_str))
        .bind(job.conclusion().as_str())
        .bind(identity.map(|identity| format!("{:032x}", identity.trace_id())))
        .bind(identity.map(|identity| format!("{:016x}", identity.span_id())))
        .bind(started_at_nanos)
        .bind(completed_at_nanos)
        .bind(job.timing().source().as_str())
        .bind(billing.map(|billing| billing.billable_minutes))
        .bind(cost.map(|cost| cost.cost_usd))
        .bind(cost.map(|cost| cost.price_table_version.as_str()))
        .bind(runner_label)
        .execute(&mut *transaction)
        .await
        .map_err(map_sqlx_error)?;
        let attempt_final = attempt_is_final(
            &mut transaction,
            repository_id,
            job.run_id(),
            job.run_attempt(),
        )
        .await
        .map_err(map_sqlx_error)?;
        transaction.commit().await.map_err(map_sqlx_error)?;
        Ok(attempt_final)
    }

    /// Lists the emitted job traces of one run attempt, oldest job start first.
    ///
    /// # Parameters
    ///
    /// * `repository_id` - The authenticated repository row identifier.
    /// * `run_id` - The validated workflow run identifier.
    /// * `run_attempt` - The validated workflow run attempt.
    /// * `limit` - The greatest number of rows to return.
    ///
    /// # Returns
    ///
    /// At most `limit` bounded job summaries. Rows whose stored values no longer satisfy the
    /// bounded telemetry vocabulary are skipped rather than failing the whole lookup.
    ///
    /// # Errors
    ///
    /// Returns [`WorkflowJobLinkStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`WorkflowJobLinkStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn list(
        &self,
        repository_id: RepositoryId,
        run_id: WorkflowRunId,
        run_attempt: WorkflowRunAttempt,
        limit: usize,
    ) -> Result<Vec<PipelineJobSummary>, WorkflowJobLinkStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::WorkflowJobLinkList,
            self.list_inner(repository_id, run_id, run_attempt, limit),
        )
        .await
    }

    async fn list_inner(
        &self,
        repository_id: RepositoryId,
        run_id: WorkflowRunId,
        run_attempt: WorkflowRunAttempt,
        limit: usize,
    ) -> Result<Vec<PipelineJobSummary>, WorkflowJobLinkStoreError> {
        let limit = i64::try_from(limit).map_err(|_| WorkflowJobLinkStoreError::Internal)?;
        let rows = sqlx::query_as::<_, StoredWorkflowJobLink>(
            "SELECT workflow_job_id, job_name, conclusion, trace_id, span_id, started_at_nanos, \
             completed_at_nanos, timing_source FROM workflow_job_links \
             WHERE repository_id = ? AND workflow_run_id = ? AND workflow_run_attempt = ? \
             AND trace_id IS NOT NULL \
             ORDER BY started_at_nanos, workflow_job_id LIMIT ?",
        )
        .bind(repository_id.get())
        .bind(run_id.get())
        .bind(run_attempt.get())
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        Ok(rows
            .into_iter()
            .filter_map(StoredWorkflowJobLink::into_summary)
            .collect())
    }

    /// Sums the job costs and billable minutes of one run attempt.
    ///
    /// # Errors
    ///
    /// Returns [`WorkflowJobLinkStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`WorkflowJobLinkStoreError::Internal`] for any other persistence failure.
    #[cfg(test)]
    pub(crate) async fn run_totals(
        &self,
        repository_id: RepositoryId,
        run_id: WorkflowRunId,
        run_attempt: WorkflowRunAttempt,
    ) -> Result<JobCostTotals, WorkflowJobLinkStoreError> {
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
    ) -> Result<JobCostTotals, sqlx::Error> {
        let (cost_usd, priced_jobs): (f64, i64) = sqlx::query_as(
            "SELECT TOTAL(cost_usd), COUNT(cost_usd) FROM workflow_job_links \
             WHERE repository_id = ? AND workflow_run_id = ? AND workflow_run_attempt = ?",
        )
        .bind(repository_id.get())
        .bind(run_id.get())
        .bind(run_attempt.get())
        .fetch_one(&mut *connection)
        .await?;
        let minutes_by_label: Vec<(String, i64)> = sqlx::query_as(
            "SELECT CASE WHEN cost_usd IS NULL THEN ? ELSE coalesce(runner_label, ?) END \
             AS label, SUM(billable_minutes) FROM workflow_job_links \
             WHERE repository_id = ? AND workflow_run_id = ? AND workflow_run_attempt = ? \
             AND billable_minutes IS NOT NULL GROUP BY label",
        )
        .bind(UNPRICED_RUNNER_LABEL_KEY)
        .bind(UNLABELLED_RUNNER_LABEL_KEY)
        .bind(repository_id.get())
        .bind(run_id.get())
        .bind(run_attempt.get())
        .fetch_all(&mut *connection)
        .await?;
        // Two versions are enough to tell that the attempt was priced by more than one table.
        let price_table_versions: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT price_table_version FROM workflow_job_links \
             WHERE repository_id = ? AND workflow_run_id = ? AND workflow_run_attempt = ? \
             AND price_table_version IS NOT NULL LIMIT 2",
        )
        .bind(repository_id.get())
        .bind(run_id.get())
        .bind(run_attempt.get())
        .fetch_all(&mut *connection)
        .await?;
        Ok(JobCostTotals {
            cost_usd: (priced_jobs > 0).then_some(cost_usd),
            minutes_by_label,
            price_table_versions,
        })
    }

    /// Deletes at most 1,000 workflow-job links last updated before `cutoff`.
    ///
    /// # Errors
    ///
    /// Returns [`WorkflowJobLinkStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`WorkflowJobLinkStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn prune_batch(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<u64, WorkflowJobLinkStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::WorkflowJobLinkPrune,
            self.prune_batch_inner(cutoff),
        )
        .await
    }

    async fn prune_batch_inner(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<u64, WorkflowJobLinkStoreError> {
        let cutoff = cutoff
            .to_offset(UtcOffset::UTC)
            .format(WORKFLOW_JOB_LINK_TIMESTAMP_FORMAT)
            .map_err(|_| WorkflowJobLinkStoreError::Internal)?;
        let result = sqlx::query(
            "DELETE FROM workflow_job_links WHERE rowid IN (\
                 SELECT rowid FROM workflow_job_links WHERE updated_at < ? \
                 ORDER BY updated_at, rowid LIMIT ?\
             )",
        )
        .bind(cutoff)
        .bind(WORKFLOW_JOB_LINK_PRUNE_BATCH_SIZE)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(result.rows_affected())
    }
}

/// The summed job billing of one run attempt.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct JobCostTotals {
    /// The summed job cost, or `None` when no job of the attempt was priced.
    pub(crate) cost_usd: Option<f64>,
    /// Billable minutes per normalized price-table label; `None` is the default rate.
    pub(crate) minutes_by_label: Vec<(String, i64)>,
    /// At most two distinct price-table versions of the attempt's priced jobs.
    pub(crate) price_table_versions: Vec<String>,
}

#[derive(FromRow)]
struct StoredWorkflowJobLink {
    workflow_job_id: i64,
    job_name: Option<String>,
    conclusion: String,
    trace_id: String,
    span_id: String,
    started_at_nanos: i64,
    completed_at_nanos: i64,
    timing_source: String,
}

impl StoredWorkflowJobLink {
    /// Revalidates one stored row against the bounded telemetry vocabulary.
    ///
    /// Returns `None` for any row that no longer projects into a bounded summary, so a single
    /// corrupted row cannot suppress an entire pipeline-run trace.
    fn into_summary(self) -> Option<PipelineJobSummary> {
        let job_id = WorkflowJobId::new(self.workflow_job_id).ok()?;
        let conclusion = normalize_stored_conclusion(&self.conclusion)?;
        let identity = WorkflowJobTraceIdentity::new(
            TraceId::from_hex(&self.trace_id).ok()?,
            SpanId::from_hex(&self.span_id).ok()?,
        );
        if !identity.is_valid() {
            return None;
        }
        let start = nanos_to_system_time(self.started_at_nanos)?;
        let end = nanos_to_system_time(self.completed_at_nanos)?;
        let source = match self.timing_source.as_str() {
            "reported" => TimingSource::Reported,
            "fallback" => TimingSource::Fallback,
            _ => return None,
        };
        let timing = HistoricalTiming::derived(start, end, source).ok()?;

        Some(PipelineJobSummary::new(
            job_id,
            self.job_name.as_deref().and_then(DisplayName::sanitize),
            conclusion,
            timing,
            identity,
        ))
    }
}

/// Rejects any stored conclusion outside the bounded vocabulary rather than normalizing it.
///
/// [`WorkflowConclusion::normalize`] maps unknown text to `other`; here an unexpected value means
/// the row was tampered with, so the row is dropped instead.
fn normalize_stored_conclusion(value: &str) -> Option<WorkflowConclusion> {
    let conclusion = WorkflowConclusion::normalize(Some(value));
    (conclusion.as_str() == value).then_some(conclusion)
}

fn system_time_to_nanos(value: SystemTime) -> Option<i64> {
    value
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_nanos()).ok())
}

fn nanos_to_system_time(nanos: i64) -> Option<SystemTime> {
    u64::try_from(nanos)
        .ok()
        .map(|nanos| UNIX_EPOCH + Duration::from_nanos(nanos))
}

/// A stable, redacted workflow-job link persistence failure.
#[derive(Clone, Copy, Error, PartialEq, Eq)]
pub(crate) enum WorkflowJobLinkStoreError {
    /// SQLite is temporarily busy or locked.
    #[error("workflow-job link storage is temporarily unavailable")]
    Unavailable,
    /// SQLite returned an unexpected persistence failure whose details were discarded.
    #[error("internal workflow-job link persistence failure")]
    Internal,
}

impl fmt::Debug for WorkflowJobLinkStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "WorkflowJobLinkStoreError::Unavailable",
            Self::Internal => "WorkflowJobLinkStoreError::Internal",
        })
    }
}

fn map_sqlx_error(error: sqlx::Error) -> WorkflowJobLinkStoreError {
    if sqlite_is_busy_or_locked(&error) {
        WorkflowJobLinkStoreError::Unavailable
    } else {
        WorkflowJobLinkStoreError::Internal
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use opentelemetry::trace::{SpanId, TraceId};
    use sqlx::{Row, SqlitePool};
    use time::macros::datetime;

    use super::WorkflowJobLinkStore;
    use crate::{
        domain::{delivery::DeliveryId, repository::RepositoryId},
        security::CanonicalRepositoryName,
        storage::open_database,
        telemetry::{
            pipeline::WorkflowJobTraceIdentity,
            workflow::{
                DisplayName, HistoricalTiming, TimingSource, WorkflowConclusion,
                WorkflowJobBilling, WorkflowJobCost, WorkflowJobId, WorkflowJobTrace,
                WorkflowJobTraceParts, WorkflowPullRequests, WorkflowRunAttempt, WorkflowRunId,
            },
        },
    };

    const DELIVERY: &str = "550e8400-e29b-41d4-a716-446655440000";

    fn identity(seed: u128) -> WorkflowJobTraceIdentity {
        WorkflowJobTraceIdentity::new(
            TraceId::from(seed),
            SpanId::from_bytes((seed as u64).to_be_bytes()),
        )
    }

    fn instant(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn job(
        job_id: i64,
        name: Option<&str>,
        conclusion: WorkflowConclusion,
        timing: HistoricalTiming,
    ) -> WorkflowJobTrace {
        WorkflowJobTrace::new(WorkflowJobTraceParts {
            repository_name: CanonicalRepositoryName::new("owner/repo")
                .expect("repository name is canonical"),
            delivery_id: DeliveryId::parse(DELIVERY).expect("delivery id parses"),
            workflow_name: DisplayName::sanitize("Build"),
            run_id: WorkflowRunId::new(31).expect("run id is positive"),
            run_attempt: WorkflowRunAttempt::new(2).expect("attempt is positive"),
            job_id: WorkflowJobId::new(job_id).expect("job id is positive"),
            job_name: name.and_then(DisplayName::sanitize),
            conclusion,
            head_sha: None,
            pull_requests: WorkflowPullRequests::new([]),
            workflow_run_context: None,
            required: None,
            timing,
            steps: Vec::new(),
        })
    }

    async fn insert_repository(pool: &SqlitePool) -> RepositoryId {
        let id = sqlx::query_scalar(
            "INSERT INTO repositories (full_name, webhook_secret_ciphertext, \
             webhook_secret_nonce, encryption_version, enabled, created_at, updated_at) \
             VALUES ('owner/repo', X'01', X'02', 1, 1, \
             '2026-08-19T10:00:00.000Z', '2026-08-19T10:00:00.000Z') RETURNING id",
        )
        .fetch_one(pool)
        .await
        .expect("repository inserts");
        RepositoryId::new(id).expect("repository id is positive")
    }

    async fn store_with_repository() -> (
        tempfile::TempDir,
        SqlitePool,
        WorkflowJobLinkStore,
        RepositoryId,
    ) {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens");
        let repository_id = insert_repository(&pool).await;
        let store = WorkflowJobLinkStore::new(pool.clone());
        (directory, pool, store, repository_id)
    }

    fn run_id() -> WorkflowRunId {
        WorkflowRunId::new(31).expect("run id is positive")
    }

    fn run_attempt() -> WorkflowRunAttempt {
        WorkflowRunAttempt::new(2).expect("attempt is positive")
    }

    #[tokio::test]
    async fn priced_links_survive_the_nullable_trace_rebuild() {
        const REBUILD_VERSION: i64 = 202_611_180_002;
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let path = directory.path().join("exporter.sqlite3");
        let pool = SqlitePool::connect_with(
            sqlx::sqlite::SqliteConnectOptions::new()
                .filename(&path)
                .create_if_missing(true)
                .foreign_keys(true),
        )
        .await
        .expect("database opens");
        let mut earlier = sqlx::migrate!();
        earlier.migrations = earlier
            .migrations
            .iter()
            .filter(|migration| migration.version < REBUILD_VERSION)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        earlier.run(&pool).await.expect("earlier migrations apply");
        let repository_id = insert_repository(&pool).await;
        sqlx::query(
            "INSERT INTO workflow_job_links (repository_id, workflow_run_id, \
             workflow_run_attempt, workflow_job_id, job_name, conclusion, trace_id, span_id, \
             started_at_nanos, completed_at_nanos, timing_source, updated_at, billable_minutes, \
             cost_usd, price_table_version) \
             VALUES (?, 31, 2, 7, 'build', 'success', ?, ?, 100, 220, 'reported', \
             '2026-11-13T10:00:00.000Z', 2, 0.012, '0123456789ab')",
        )
        .bind(repository_id.get())
        .bind(format!("{:032x}", 1_u128))
        .bind(format!("{:016x}", 1_u64))
        .execute(&pool)
        .await
        .expect("pre-rebuild link inserts");
        pool.close().await;

        let migrated = open_database(&path)
            .await
            .expect("remaining migrations apply");
        let store = WorkflowJobLinkStore::new(migrated.clone());

        let totals = store
            .run_totals(repository_id, run_id(), run_attempt())
            .await
            .expect("totals succeed");
        assert_eq!(totals.cost_usd, Some(0.012));
        assert_eq!(totals.minutes_by_label, [("unlabelled".to_owned(), 2)]);
        assert_eq!(totals.price_table_versions, ["0123456789ab"]);
        let summaries = store
            .list(repository_id, run_id(), run_attempt(), 10)
            .await
            .expect("lookup succeeds");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].conclusion(), WorkflowConclusion::Success);
        let runner_label: Option<String> =
            sqlx::query_scalar("SELECT runner_label FROM workflow_job_links")
                .fetch_one(&migrated)
                .await
                .expect("link row loads");
        assert_eq!(runner_label, None);
    }

    #[tokio::test]
    async fn migration_contains_only_bounded_link_fields() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens");

        let columns = sqlx::query("PRAGMA table_info(workflow_job_links)")
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
                "workflow_run_id",
                "workflow_run_attempt",
                "workflow_job_id",
                "job_name",
                "conclusion",
                "trace_id",
                "span_id",
                "started_at_nanos",
                "completed_at_nanos",
                "timing_source",
                "updated_at",
                "billable_minutes",
                "cost_usd",
                "price_table_version",
                "runner_label",
            ]
        );
        for forbidden in [
            "payload",
            "signature",
            "actor",
            "html_url",
            "logs",
            "steps",
            "output",
            "command",
        ] {
            assert!(!columns.iter().any(|column| column == forbidden));
        }
    }

    #[tokio::test]
    async fn recorded_links_survive_reopen_and_list_oldest_job_start_first() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let path = directory.path().join("exporter.sqlite3");
        let pool = open_database(&path).await.expect("database opens");
        let repository_id = insert_repository(&pool).await;
        let store = WorkflowJobLinkStore::new(pool.clone());

        store
            .record(
                repository_id,
                &job(
                    41,
                    Some("Linux\tJob"),
                    WorkflowConclusion::Success,
                    HistoricalTiming::reported(instant(300), instant(400))
                        .expect("timing is ordered"),
                ),
                Some(identity(41)),
            )
            .await
            .expect("first link persists");
        store
            .record(
                repository_id,
                &job(
                    42,
                    None,
                    WorkflowConclusion::Failure,
                    HistoricalTiming::fallback(instant(100)),
                ),
                Some(identity(42)),
            )
            .await
            .expect("second link persists");
        pool.close().await;

        let reopened = open_database(&path).await.expect("database reopens");
        let summaries = WorkflowJobLinkStore::new(reopened)
            .list(repository_id, run_id(), run_attempt(), 10)
            .await
            .expect("lookup succeeds");

        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].conclusion(), WorkflowConclusion::Failure);
        assert_eq!(summaries[0].timing().source(), TimingSource::Fallback);
        assert_eq!(summaries[0].timing().start(), instant(100));
        assert_eq!(summaries[1].conclusion(), WorkflowConclusion::Success);
        assert_eq!(summaries[1].timing().source(), TimingSource::Reported);
        assert_eq!(summaries[1].timing().end(), instant(400));
    }

    #[tokio::test]
    async fn recording_the_same_job_twice_replaces_the_earlier_identity() {
        let (_directory, _pool, store, repository_id) = store_with_repository().await;
        let timing =
            HistoricalTiming::reported(instant(100), instant(200)).expect("timing is ordered");

        store
            .record(
                repository_id,
                &job(41, None, WorkflowConclusion::Success, timing.clone()),
                Some(identity(41)),
            )
            .await
            .expect("first link persists");
        store
            .record(
                repository_id,
                &job(41, None, WorkflowConclusion::Failure, timing),
                Some(identity(99)),
            )
            .await
            .expect("replacement link persists");

        let summaries = store
            .list(repository_id, run_id(), run_attempt(), 10)
            .await
            .expect("lookup succeeds");
        assert_eq!(summaries.len(), 1);
        assert_eq!(summaries[0].conclusion(), WorkflowConclusion::Failure);
    }

    #[tokio::test]
    async fn listing_is_scoped_to_one_run_attempt_and_honors_the_limit() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;
        for job_id in 1..=3 {
            store
                .record(
                    repository_id,
                    &job(
                        job_id,
                        None,
                        WorkflowConclusion::Success,
                        HistoricalTiming::reported(
                            instant(u64::try_from(job_id).expect("job id is positive")),
                            instant(100),
                        )
                        .expect("timing is ordered"),
                    ),
                    Some(identity(
                        u128::try_from(job_id).expect("job id is positive"),
                    )),
                )
                .await
                .expect("link persists");
        }
        sqlx::query(
            "UPDATE workflow_job_links SET workflow_run_attempt = 1 WHERE workflow_job_id = 3",
        )
        .execute(&pool)
        .await
        .expect("other attempt fixture updates");

        assert_eq!(
            store
                .list(repository_id, run_id(), run_attempt(), 10)
                .await
                .expect("lookup succeeds")
                .len(),
            2
        );
        assert_eq!(
            store
                .list(repository_id, run_id(), run_attempt(), 1)
                .await
                .expect("lookup succeeds")
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn tampered_rows_are_skipped_without_suppressing_the_run() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;
        for job_id in 1..=2 {
            store
                .record(
                    repository_id,
                    &job(
                        job_id,
                        None,
                        WorkflowConclusion::Success,
                        HistoricalTiming::reported(
                            instant(u64::try_from(job_id).expect("job id is positive")),
                            instant(100),
                        )
                        .expect("timing is ordered"),
                    ),
                    Some(identity(
                        u128::try_from(job_id).expect("job id is positive"),
                    )),
                )
                .await
                .expect("link persists");
        }
        sqlx::query(
            "UPDATE workflow_job_links SET trace_id = '00000000000000000000000000000000' \
             WHERE workflow_job_id = 1",
        )
        .execute(&pool)
        .await
        .expect("invalid trace fixture updates");

        let summaries = store
            .list(repository_id, run_id(), run_attempt(), 10)
            .await
            .expect("lookup degrades safely");
        assert_eq!(summaries.len(), 1);
    }

    #[tokio::test]
    async fn prune_uses_update_time_and_is_bounded_to_one_thousand_rows_per_call() {
        let (_directory, pool, store, _repository_id) = store_with_repository().await;
        sqlx::query(
            "WITH RECURSIVE sequence(value) AS (\
                 VALUES(1) UNION ALL SELECT value + 1 FROM sequence WHERE value < 1005\
             ) INSERT INTO workflow_job_links \
             (repository_id, workflow_run_id, workflow_run_attempt, workflow_job_id, conclusion, \
              trace_id, span_id, started_at_nanos, completed_at_nanos, timing_source, updated_at) \
             SELECT 1, value, 1, 1, 'success', '000000000000000000000000000000ff', \
             '00000000000000ff', 0, 1, 'reported', '2020-01-01T00:00:00.000Z' FROM sequence",
        )
        .execute(&pool)
        .await
        .expect("expired links insert");

        assert_eq!(
            store
                .prune_batch(datetime!(2026-08-19 10:00:00 UTC))
                .await
                .expect("first prune succeeds"),
            1_000
        );
        assert_eq!(
            store
                .prune_batch(datetime!(2026-08-19 10:00:00 UTC))
                .await
                .expect("second prune succeeds"),
            5
        );
    }

    #[tokio::test]
    async fn billing_columns_store_minutes_and_cost_only_when_priced() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;
        let timing =
            HistoricalTiming::reported(instant(0), instant(300)).expect("timing is ordered");
        let billing = |cost| WorkflowJobBilling {
            runner_labels: Vec::new(),
            runner_group: None,
            runner_name: None,
            billable_minutes: 5,
            cost,
        };
        let priced = job(41, None, WorkflowConclusion::Success, timing.clone()).with_billing(
            billing(Some(WorkflowJobCost {
                cost_usd: 0.4,
                price_table_version: "0123456789ab".to_owned(),
                fell_back: false,
                usd_per_minute: Some(0.08),
                price_label: Some("ubuntu-24.04-xl".to_owned()),
            })),
        );
        let unpriced =
            job(42, None, WorkflowConclusion::Success, timing).with_billing(billing(None));

        for (job, seed) in [(&priced, 41), (&unpriced, 42)] {
            store
                .record(repository_id, job, Some(identity(seed)))
                .await
                .expect("link persists");
        }
        type BillingRow = (i64, Option<i64>, Option<f64>, Option<String>);
        let rows: Vec<BillingRow> = sqlx::query_as(
            "SELECT workflow_job_id, billable_minutes, cost_usd, price_table_version \
             FROM workflow_job_links ORDER BY workflow_job_id",
        )
        .fetch_all(&pool)
        .await
        .expect("rows load");

        assert_eq!(
            rows,
            [
                (41, Some(5), Some(0.4), Some("0123456789ab".to_owned())),
                (42, Some(5), None, None),
            ]
        );
    }

    #[tokio::test]
    async fn unknown_repository_and_invalid_identity_failures_are_redacted() {
        let (_directory, _pool, store, repository_id) = store_with_repository().await;
        let timing =
            HistoricalTiming::reported(instant(100), instant(200)).expect("timing is ordered");

        let foreign_key_error = store
            .record(
                RepositoryId::new(999).expect("repository id is positive"),
                &job(41, None, WorkflowConclusion::Success, timing.clone()),
                Some(identity(41)),
            )
            .await
            .expect_err("unknown repository fails");
        assert_eq!(
            format!("{foreign_key_error:?}"),
            "WorkflowJobLinkStoreError::Internal"
        );
        assert!(!format!("{foreign_key_error}").contains("FOREIGN KEY"));

        let identity_error = store
            .record(
                repository_id,
                &job(41, None, WorkflowConclusion::Success, timing),
                Some(WorkflowJobTraceIdentity::new(
                    TraceId::INVALID,
                    SpanId::INVALID,
                )),
            )
            .await
            .expect_err("an unexported identity is never persisted");
        assert_eq!(
            format!("{identity_error:?}"),
            "WorkflowJobLinkStoreError::Internal"
        );
    }

    #[tokio::test]
    async fn untraced_jobs_count_toward_run_totals_but_never_link_a_pipeline_span() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;
        let timing =
            HistoricalTiming::reported(instant(0), instant(300)).expect("timing is ordered");
        let priced = |job_id, minutes, cost_usd, label: Option<&str>| {
            job(job_id, None, WorkflowConclusion::Success, timing.clone()).with_billing(
                WorkflowJobBilling {
                    runner_labels: Vec::new(),
                    runner_group: None,
                    runner_name: None,
                    billable_minutes: minutes,
                    cost: Some(WorkflowJobCost {
                        cost_usd,
                        price_table_version: "0123456789ab".to_owned(),
                        fell_back: label.is_none(),
                        usd_per_minute: Some(0.01),
                        price_label: label.map(str::to_owned),
                    }),
                },
            )
        };
        store
            .record(
                repository_id,
                &priced(41, 5, 0.4, Some("Ubuntu-24.04-XL")),
                Some(identity(41)),
            )
            .await
            .expect("traced job persists");
        store
            .record(repository_id, &priced(42, 3, 0.03, None), None)
            .await
            .expect("untraced job persists");
        store
            .record(
                repository_id,
                &job(44, None, WorkflowConclusion::Success, timing.clone()).with_billing(
                    WorkflowJobBilling {
                        runner_labels: Vec::new(),
                        runner_group: None,
                        runner_name: None,
                        billable_minutes: 4,
                        cost: None,
                    },
                ),
                None,
            )
            .await
            .expect("unpriced job persists");
        store
            .record(repository_id, &priced(43, 7, 0.07, None), None)
            .await
            .expect("other attempt persists");
        sqlx::query(
            "UPDATE workflow_job_links SET workflow_run_attempt = 1 WHERE workflow_job_id = 43",
        )
        .execute(&pool)
        .await
        .expect("other attempt fixture updates");

        let links = store
            .list(repository_id, run_id(), run_attempt(), 10)
            .await
            .expect("lookup succeeds");
        assert_eq!(links.len(), 1, "only the traced job links a pipeline span");
        let totals = store
            .run_totals(repository_id, run_id(), run_attempt())
            .await
            .expect("totals load");
        let mut minutes = totals.minutes_by_label.clone();
        minutes.sort();
        assert_eq!(
            minutes,
            [
                ("default".to_owned(), 3),
                ("ubuntu_24_04_xl".to_owned(), 5),
                ("unpriced".to_owned(), 4),
            ]
        );
        assert!((totals.cost_usd.expect("jobs are priced") - 0.43).abs() < 1e-9);
        assert_eq!(totals.price_table_versions, ["0123456789ab"]);

        let empty = store
            .run_totals(
                repository_id,
                WorkflowRunId::new(99).expect("run id is positive"),
                run_attempt(),
            )
            .await
            .expect("totals load");
        assert_eq!(empty.cost_usd, None);
        assert!(empty.minutes_by_label.is_empty());
        assert!(empty.price_table_versions.is_empty());
    }
}
