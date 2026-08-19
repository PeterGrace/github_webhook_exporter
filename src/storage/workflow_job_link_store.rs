use std::{
    fmt,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use opentelemetry::trace::{SpanId, TraceId};
use sqlx::{FromRow, SqlitePool};
use thiserror::Error;
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime, UtcOffset};

use crate::{
    domain::repository::RepositoryId,
    telemetry::{
        pipeline::{PipelineJobSummary, WorkflowJobTraceIdentity},
        trace::{self, DatabaseOperation},
        workflow::{
            DisplayName, HistoricalTiming, TimingSource, WorkflowConclusion, WorkflowJobId,
            WorkflowJobTrace, WorkflowRunAttempt, WorkflowRunId,
        },
    },
};

use super::sqlite_is_busy_or_locked;

const WORKFLOW_JOB_LINK_PRUNE_BATCH_SIZE: i64 = 1_000;
const WORKFLOW_JOB_LINK_TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// Durable identities of emitted workflow-job traces, used to link pipeline-run summaries.
///
/// Only what a pipeline-run summary span needs is retained: the exported trace and span
/// identifiers, the sanitized job name, the bounded conclusion, and the historical interval that
/// the job trace itself used.
#[derive(Clone)]
pub(crate) struct WorkflowJobLinkStore {
    pool: SqlitePool,
}

impl WorkflowJobLinkStore {
    /// Creates a workflow-job link store from an already migrated SQLite pool.
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Inserts or replaces the exported identity of one emitted workflow-job trace.
    ///
    /// # Parameters
    ///
    /// * `repository_id` - The authenticated repository row identifier.
    /// * `job` - The bounded job trace that was just emitted.
    /// * `identity` - The exported identity of that job trace's root span.
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
        identity: WorkflowJobTraceIdentity,
    ) -> Result<(), WorkflowJobLinkStoreError> {
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
        identity: WorkflowJobTraceIdentity,
    ) -> Result<(), WorkflowJobLinkStoreError> {
        if !identity.is_valid() {
            return Err(WorkflowJobLinkStoreError::Internal);
        }
        let started_at_nanos = system_time_to_nanos(job.timing().start())
            .ok_or(WorkflowJobLinkStoreError::Internal)?;
        let completed_at_nanos =
            system_time_to_nanos(job.timing().end()).ok_or(WorkflowJobLinkStoreError::Internal)?;

        sqlx::query(
            "INSERT INTO workflow_job_links \
             (repository_id, workflow_run_id, workflow_run_attempt, workflow_job_id, job_name, \
              conclusion, trace_id, span_id, started_at_nanos, completed_at_nanos, timing_source, \
              updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) \
             ON CONFLICT(repository_id, workflow_run_id, workflow_run_attempt, workflow_job_id) \
             DO UPDATE SET job_name = excluded.job_name, conclusion = excluded.conclusion, \
             trace_id = excluded.trace_id, span_id = excluded.span_id, \
             started_at_nanos = excluded.started_at_nanos, \
             completed_at_nanos = excluded.completed_at_nanos, \
             timing_source = excluded.timing_source, updated_at = excluded.updated_at",
        )
        .bind(repository_id.get())
        .bind(job.run_id().get())
        .bind(job.run_attempt().get())
        .bind(job.job_id().get())
        .bind(job.job_name().map(DisplayName::as_str))
        .bind(job.conclusion().as_str())
        .bind(format!("{:032x}", identity.trace_id()))
        .bind(format!("{:016x}", identity.span_id()))
        .bind(started_at_nanos)
        .bind(completed_at_nanos)
        .bind(job.timing().source().as_str())
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(())
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
                DisplayName, HistoricalTiming, TimingSource, WorkflowConclusion, WorkflowJobId,
                WorkflowJobTrace, WorkflowJobTraceParts, WorkflowPullRequests, WorkflowRunAttempt,
                WorkflowRunId,
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
                identity(41),
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
                identity(42),
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
                identity(41),
            )
            .await
            .expect("first link persists");
        store
            .record(
                repository_id,
                &job(41, None, WorkflowConclusion::Failure, timing),
                identity(99),
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
                    identity(u128::try_from(job_id).expect("job id is positive")),
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
                    identity(u128::try_from(job_id).expect("job id is positive")),
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
    async fn unknown_repository_and_invalid_identity_failures_are_redacted() {
        let (_directory, _pool, store, repository_id) = store_with_repository().await;
        let timing =
            HistoricalTiming::reported(instant(100), instant(200)).expect("timing is ordered");

        let foreign_key_error = store
            .record(
                RepositoryId::new(999).expect("repository id is positive"),
                &job(41, None, WorkflowConclusion::Success, timing.clone()),
                identity(41),
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
                WorkflowJobTraceIdentity::new(TraceId::INVALID, SpanId::INVALID),
            )
            .await
            .expect_err("an unexported identity is never persisted");
        assert_eq!(
            format!("{identity_error:?}"),
            "WorkflowJobLinkStoreError::Internal"
        );
    }
}
