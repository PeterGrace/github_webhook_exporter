use std::fmt;

use sqlx::{FromRow, SqliteConnection, SqlitePool};
use thiserror::Error;
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime, UtcOffset};

use crate::{
    domain::{merge_queue::PullRequestNumber, repository::RepositoryId},
    security::CanonicalRepositoryName,
    telemetry::{
        run_cost::PullRequestResolution,
        trace::{self, CommitSha, DatabaseOperation},
        workflow::{
            DisplayName, WorkflowConclusion, WorkflowEvent, WorkflowFile, WorkflowRunAttempt,
            WorkflowRunId,
        },
    },
};

use super::{
    pull_request_cost_store::record_cost_activity_in, sqlite_is_busy_or_locked, CiUsageStore,
    JobCostTotals, UsageTotals, WorkflowJobLinkStore,
};

const RUN_COST_PRUNE_BATCH_SIZE: i64 = 1_000;
const RUN_COST_TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// What a completed `workflow_run` delivery tells the finalizer about one run attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PendingRunCost {
    pub(crate) run_id: WorkflowRunId,
    pub(crate) run_attempt: WorkflowRunAttempt,
    pub(crate) event: WorkflowEvent,
    pub(crate) workflow_name: Option<DisplayName>,
    pub(crate) workflow_file: Option<WorkflowFile>,
    pub(crate) conclusion: WorkflowConclusion,
    pub(crate) head_sha: Option<CommitSha>,
    /// The pull request the payload resolved, and the rule that matched.
    pub(crate) pull_request: Option<(PullRequestNumber, PullRequestResolution)>,
}

/// One pending run attempt whose grace period has elapsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DueRunCost {
    pub(crate) repository_id: RepositoryId,
    pub(crate) repository_name: CanonicalRepositoryName,
    pub(crate) pending: PendingRunCost,
}

/// The attempts one finalizer poll found due.
#[derive(Debug, Default)]
pub(crate) struct DueRunCosts {
    pub(crate) runs: Vec<DueRunCost>,
    /// Rows that no longer parsed and were set aside for good.
    pub(crate) abandoned: usize,
    /// Rows that no longer parsed but could not be set aside, so a later poll reads them again.
    pub(crate) abandon_failed: usize,
}

/// The total and attribution a run attempt is finalized with.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct FinalizedRunCost {
    pub(crate) pull_request: Option<PullRequestNumber>,
    pub(crate) resolution: PullRequestResolution,
    pub(crate) total_usd: f64,
    pub(crate) price_table_version: Option<String>,
    pub(crate) event: WorkflowEvent,
    pub(crate) head_sha: Option<CommitSha>,
    /// Summed USD per stored source, before any bounding, so pull-request records can re-sum it.
    pub(crate) cost_by_source: Vec<(String, f64)>,
}

/// One run attempt this call finalized.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RunFinalization<T> {
    /// What the caller's builder returned.
    pub(crate) value: T,
    /// Whether the attempt's pull request already had a final record, so its cost is late there.
    pub(crate) pull_request_was_final: bool,
}

/// Durable run-cost finalizations and the finalized totals attributed to pull requests.
///
/// A finalization row is written once per run attempt when its `workflow_run.completed` delivery
/// arrives and stays pending until the background finalizer totals it. Pull-request totals are
/// written in the same transaction that finalizes the row, so a run is never counted for its pull
/// request without being final.
#[derive(Clone)]
pub(crate) struct RunCostStore {
    pool: SqlitePool,
}

impl RunCostStore {
    /// Creates a run-cost store from an already migrated SQLite pool.
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Schedules one run attempt for finalization at `due_at`, unless it is already scheduled.
    ///
    /// # Returns
    ///
    /// `true` when this call scheduled the attempt, `false` when an earlier delivery already had.
    ///
    /// # Errors
    ///
    /// Returns [`RunCostStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`RunCostStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn schedule(
        &self,
        repository_id: RepositoryId,
        pending: &PendingRunCost,
        due_at: OffsetDateTime,
    ) -> Result<bool, RunCostStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::RunCostSchedule,
            self.schedule_inner(repository_id, pending, due_at),
        )
        .await
    }

    async fn schedule_inner(
        &self,
        repository_id: RepositoryId,
        pending: &PendingRunCost,
        due_at: OffsetDateTime,
    ) -> Result<bool, RunCostStoreError> {
        let (pull_request, resolution) = pending.pull_request.map_or(
            (None, PullRequestResolution::None),
            |(number, resolution)| (Some(number.get()), resolution),
        );
        let result = sqlx::query(
            "INSERT INTO run_cost_finalizations \
             (repository_id, workflow_run_id, workflow_run_attempt, event, workflow_name, \
              workflow_file, conclusion, head_sha, pull_request_number, pull_request_resolution, \
              due_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) \
             ON CONFLICT(repository_id, workflow_run_id, workflow_run_attempt) DO NOTHING",
        )
        .bind(repository_id.get())
        .bind(pending.run_id.get())
        .bind(pending.run_attempt.get())
        .bind(pending.event.as_str())
        .bind(pending.workflow_name.as_ref().map(DisplayName::as_str))
        .bind(pending.workflow_file.as_ref().map(WorkflowFile::as_str))
        .bind(pending.conclusion.as_str())
        .bind(pending.head_sha.as_ref().map(CommitSha::as_str))
        .bind(pull_request)
        .bind(resolution.as_str())
        .bind(format_timestamp(due_at)?)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(result.rows_affected() == 1)
    }

    /// Lists at most `limit` pending attempts due at or before `now`, earliest first.
    ///
    /// This runs on every finalizer poll, so unlike the other operations it records no database
    /// span: an idle poll would otherwise export a root span every interval. Rows whose stored
    /// values no longer satisfy the bounded vocabulary are abandoned, so no later poll reads them.
    ///
    /// # Errors
    ///
    /// Returns [`RunCostStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`RunCostStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn due(
        &self,
        now: OffsetDateTime,
        limit: i64,
    ) -> Result<DueRunCosts, RunCostStoreError> {
        let rows = sqlx::query_as::<_, StoredPendingRunCost>(
            "SELECT pending.rowid AS row_id, pending.repository_id, repositories.full_name, \
             pending.workflow_run_id, pending.workflow_run_attempt, pending.event, \
             pending.workflow_name, pending.workflow_file, pending.conclusion, pending.head_sha, \
             pending.pull_request_number, pending.pull_request_resolution \
             FROM run_cost_finalizations AS pending \
             JOIN repositories ON repositories.id = pending.repository_id \
             WHERE pending.finalized_at IS NULL AND pending.abandoned_at IS NULL \
             AND pending.due_at <= ? \
             ORDER BY pending.due_at, pending.rowid LIMIT ?",
        )
        .bind(format_timestamp(now)?)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        let mut due = DueRunCosts::default();
        for row in rows {
            let row_id = row.row_id;
            match row.into_due() {
                Some(run) => due.runs.push(run),
                None => {
                    let abandoned = sqlx::query(
                        "UPDATE run_cost_finalizations SET \
                         abandoned_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE rowid = ?",
                    )
                    .bind(row_id)
                    .execute(&self.pool)
                    .await;
                    match abandoned {
                        Ok(_) => due.abandoned += 1,
                        Err(_) => due.abandon_failed += 1,
                    }
                }
            }
        }
        Ok(due)
    }

    /// Pushes a pending attempt's due time back after a failed finalization.
    ///
    /// The delay doubles with each failure, from 30 seconds up to one hour. `updated_at` is left
    /// unchanged, so retention still removes an attempt that never succeeds.
    ///
    /// # Errors
    ///
    /// Returns [`RunCostStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`RunCostStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn defer(
        &self,
        repository_id: RepositoryId,
        run_id: WorkflowRunId,
        run_attempt: WorkflowRunAttempt,
        now: OffsetDateTime,
    ) -> Result<(), RunCostStoreError> {
        trace::instrument_database_operation(DatabaseOperation::RunCostDefer, async {
            sqlx::query(
                "UPDATE run_cost_finalizations SET failed_attempts = failed_attempts + 1, \
                 due_at = strftime('%Y-%m-%dT%H:%M:%fZ', ?, \
                     '+' || min(30 * (1 << min(failed_attempts, 7)), 3600) || ' seconds') \
                 WHERE repository_id = ? AND workflow_run_id = ? AND workflow_run_attempt = ? \
                 AND finalized_at IS NULL",
            )
            .bind(format_timestamp(now)?)
            .bind(repository_id.get())
            .bind(run_id.get())
            .bind(run_attempt.get())
            .execute(&self.pool)
            .await
            .map_err(map_sqlx_error)?;
            Ok(())
        })
        .await
    }

    /// Sums one pending attempt's job and usage costs, then finalizes it with what `build` makes of
    /// those sums and, when it has a pull request, stores its total against that pull request.
    ///
    /// The sums and the finalization share one transaction that takes SQLite's write lock before
    /// reading, so a job link or usage entry either commits before the sums and is counted, or
    /// commits after the attempt is final and sees it as final.
    ///
    /// # Returns
    ///
    /// The value `build` returned when this call finalized the attempt, or `None` when it was
    /// already final.
    ///
    /// # Errors
    ///
    /// Returns [`RunCostStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`RunCostStoreError::Internal`] for any other persistence failure. On error nothing is
    /// stored and the attempt stays pending.
    pub(crate) async fn finalize<T>(
        &self,
        repository_id: RepositoryId,
        run_id: WorkflowRunId,
        run_attempt: WorkflowRunAttempt,
        build: impl FnOnce(JobCostTotals, UsageTotals) -> (FinalizedRunCost, T),
    ) -> Result<Option<RunFinalization<T>>, RunCostStoreError> {
        trace::instrument_database_operation(DatabaseOperation::RunCostFinalize, async {
            let mut transaction = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(map_sqlx_error)?;
            let jobs = trace::instrument_database_operation(
                DatabaseOperation::WorkflowJobLinkTotals,
                WorkflowJobLinkStore::run_totals_in(
                    &mut transaction,
                    repository_id,
                    run_id,
                    run_attempt,
                ),
            )
            .await
            .map_err(map_sqlx_error)?;
            let usage = trace::instrument_database_operation(
                DatabaseOperation::CiUsageTotals,
                CiUsageStore::run_totals_in(&mut transaction, repository_id, run_id, run_attempt),
            )
            .await
            .map_err(map_sqlx_error)?;
            let (finalized, value) = build(jobs, usage);
            let Some(pull_request_was_final) = finalize_in(
                &mut transaction,
                repository_id,
                run_id,
                run_attempt,
                &finalized,
            )
            .await
            .map_err(map_sqlx_error)?
            else {
                return Ok(None);
            };
            transaction.commit().await.map_err(map_sqlx_error)?;
            Ok(Some(RunFinalization {
                value,
                pull_request_was_final,
            }))
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn finalize_pending(
        &self,
        repository_id: RepositoryId,
        pending: &PendingRunCost,
        finalized: FinalizedRunCost,
    ) -> Result<bool, RunCostStoreError> {
        self.finalize(
            repository_id,
            pending.run_id,
            pending.run_attempt,
            |_, _| (finalized, ()),
        )
        .await
        .map(|finalization| finalization.is_some())
    }

    #[cfg(test)]
    pub(crate) async fn is_finalized(
        &self,
        repository_id: RepositoryId,
        run_id: WorkflowRunId,
        run_attempt: WorkflowRunAttempt,
    ) -> Result<bool, RunCostStoreError> {
        let mut connection = self.pool.acquire().await.map_err(map_sqlx_error)?;
        attempt_is_final(&mut connection, repository_id, run_id, run_attempt)
            .await
            .map_err(map_sqlx_error)
    }

    /// Deletes at most 1,000 finalizations last updated before `cutoff`.
    ///
    /// # Errors
    ///
    /// Returns [`RunCostStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`RunCostStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn prune_batch(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<u64, RunCostStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::RunCostPrune,
            self.prune_table_batch("run_cost_finalizations", "", cutoff),
        )
        .await
    }

    /// Deletes at most 1,000 pull-request run totals last updated before `cutoff`, with their
    /// per-source split, unless their pull request's record is still pending.
    ///
    /// # Errors
    ///
    /// Returns [`RunCostStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`RunCostStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn prune_pull_request_batch(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<u64, RunCostStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::PullRequestRunCostPrune,
            self.prune_table_batch(
                "pull_request_run_costs",
                "AND NOT EXISTS (\
                     SELECT 1 FROM pull_request_costs AS state \
                     WHERE state.repository_id = pull_request_run_costs.repository_id \
                     AND state.pull_request_number = pull_request_run_costs.pull_request_number \
                     AND state.finalized_at IS NULL)",
                cutoff,
            ),
        )
        .await
    }

    async fn prune_table_batch(
        &self,
        table: &'static str,
        keep: &'static str,
        cutoff: OffsetDateTime,
    ) -> Result<u64, RunCostStoreError> {
        // `table` and `keep` are literals above, never caller input.
        let result = sqlx::query(&format!(
            "DELETE FROM {table} WHERE rowid IN (\
                 SELECT rowid FROM {table} WHERE updated_at < ? {keep} \
                 ORDER BY updated_at, rowid LIMIT ?\
             )"
        ))
        .bind(format_timestamp(cutoff)?)
        .bind(RUN_COST_PRUNE_BATCH_SIZE)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(result.rows_affected())
    }
}

/// Returns whether one run attempt's total is already final, read on `connection` so a writer can
/// check inside its own transaction.
pub(crate) async fn attempt_is_final(
    connection: &mut SqliteConnection,
    repository_id: RepositoryId,
    run_id: WorkflowRunId,
    run_attempt: WorkflowRunAttempt,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM run_cost_finalizations \
         WHERE repository_id = ? AND workflow_run_id = ? AND workflow_run_attempt = ? \
         AND finalized_at IS NOT NULL",
    )
    .bind(repository_id.get())
    .bind(run_id.get())
    .bind(run_attempt.get())
    .fetch_optional(&mut *connection)
    .await
    .map(|row| row.is_some())
}

/// Finalizes one pending attempt and stores its total, split by source, against its pull request.
///
/// # Returns
///
/// `None` when the attempt was already final. Otherwise whether its pull request already had a
/// final record; `false` when the attempt has no pull request.
async fn finalize_in(
    connection: &mut SqliteConnection,
    repository_id: RepositoryId,
    run_id: WorkflowRunId,
    run_attempt: WorkflowRunAttempt,
    finalized: &FinalizedRunCost,
) -> Result<Option<bool>, sqlx::Error> {
    let updated = sqlx::query(
        "UPDATE run_cost_finalizations SET \
             finalized_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), total_usd = ?, \
             pull_request_number = ?, pull_request_resolution = ?, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
             WHERE repository_id = ? AND workflow_run_id = ? AND workflow_run_attempt = ? \
             AND finalized_at IS NULL",
    )
    .bind(finalized.total_usd)
    .bind(finalized.pull_request.map(PullRequestNumber::get))
    .bind(finalized.resolution.as_str())
    .bind(repository_id.get())
    .bind(run_id.get())
    .bind(run_attempt.get())
    .execute(&mut *connection)
    .await?;
    if updated.rows_affected() != 1 {
        return Ok(None);
    }
    // the first finalized total marks when run totals began; later inserts are ignored
    sqlx::query(
        "INSERT OR IGNORE INTO pull_request_cost_rollup (id, started_at) \
         VALUES (1, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
    )
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "DELETE FROM pull_request_run_costs \
             WHERE repository_id = ? AND workflow_run_id = ? AND workflow_run_attempt = ? \
             AND pull_request_number IS NOT ?",
    )
    .bind(repository_id.get())
    .bind(run_id.get())
    .bind(run_attempt.get())
    .bind(finalized.pull_request.map(PullRequestNumber::get))
    .execute(&mut *connection)
    .await?;
    let Some(pull_request) = finalized.pull_request else {
        return Ok(Some(false));
    };
    sqlx::query(
        "INSERT INTO pull_request_run_costs \
             (repository_id, pull_request_number, workflow_run_id, workflow_run_attempt, \
              total_usd, price_table_version, head_sha, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) \
             ON CONFLICT(repository_id, pull_request_number, workflow_run_id, \
                         workflow_run_attempt) \
             DO UPDATE SET total_usd = excluded.total_usd, \
             price_table_version = excluded.price_table_version, \
             head_sha = excluded.head_sha, updated_at = excluded.updated_at",
    )
    .bind(repository_id.get())
    .bind(pull_request.get())
    .bind(run_id.get())
    .bind(run_attempt.get())
    .bind(finalized.total_usd)
    .bind(finalized.price_table_version.as_deref())
    .bind(finalized.head_sha.as_ref().map(CommitSha::as_str))
    .execute(&mut *connection)
    .await?;
    sqlx::query(
        "DELETE FROM pull_request_run_cost_sources \
             WHERE repository_id = ? AND pull_request_number = ? AND workflow_run_id = ? \
             AND workflow_run_attempt = ?",
    )
    .bind(repository_id.get())
    .bind(pull_request.get())
    .bind(run_id.get())
    .bind(run_attempt.get())
    .execute(&mut *connection)
    .await?;
    for (source, cost_usd) in &finalized.cost_by_source {
        sqlx::query(
            "INSERT INTO pull_request_run_cost_sources \
                 (repository_id, pull_request_number, workflow_run_id, workflow_run_attempt, \
                  source, cost_usd) \
                 VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(repository_id.get())
        .bind(pull_request.get())
        .bind(run_id.get())
        .bind(run_attempt.get())
        .bind(source)
        .bind(cost_usd)
        .execute(&mut *connection)
        .await?;
    }
    let post_merge_commit = (finalized.event == WorkflowEvent::Push)
        .then_some(finalized.head_sha.as_ref())
        .flatten();
    record_cost_activity_in(connection, repository_id, pull_request, post_merge_commit)
        .await
        .map(Some)
}

#[derive(FromRow)]
struct StoredPendingRunCost {
    row_id: i64,
    repository_id: i64,
    full_name: String,
    workflow_run_id: i64,
    workflow_run_attempt: i64,
    event: String,
    workflow_name: Option<String>,
    workflow_file: Option<String>,
    conclusion: String,
    head_sha: Option<String>,
    pull_request_number: Option<i64>,
    pull_request_resolution: String,
}

impl StoredPendingRunCost {
    fn into_due(self) -> Option<DueRunCost> {
        let conclusion = WorkflowConclusion::normalize(Some(&self.conclusion));
        if conclusion.as_str() != self.conclusion {
            return None;
        }
        let resolution = PullRequestResolution::from_stored(&self.pull_request_resolution)?;
        let pull_request = match self.pull_request_number {
            Some(number) => Some((PullRequestNumber::new(number).ok()?, resolution)),
            None => None,
        };
        Some(DueRunCost {
            repository_id: RepositoryId::new(self.repository_id)?,
            repository_name: CanonicalRepositoryName::new(&self.full_name).ok()?,
            pending: PendingRunCost {
                run_id: WorkflowRunId::new(self.workflow_run_id).ok()?,
                run_attempt: WorkflowRunAttempt::new(self.workflow_run_attempt).ok()?,
                event: WorkflowEvent::from_stored(&self.event)?,
                workflow_name: self
                    .workflow_name
                    .as_deref()
                    .and_then(DisplayName::sanitize),
                workflow_file: self
                    .workflow_file
                    .as_deref()
                    .and_then(WorkflowFile::from_path),
                conclusion,
                head_sha: self
                    .head_sha
                    .as_deref()
                    .and_then(|sha| CommitSha::parse(sha).ok()),
                pull_request,
            },
        })
    }
}

fn format_timestamp(value: OffsetDateTime) -> Result<String, RunCostStoreError> {
    value
        .to_offset(UtcOffset::UTC)
        .format(RUN_COST_TIMESTAMP_FORMAT)
        .map_err(|_| RunCostStoreError::Internal)
}

/// A stable, redacted run-cost persistence failure.
#[derive(Clone, Copy, Error, PartialEq, Eq)]
pub(crate) enum RunCostStoreError {
    /// SQLite is temporarily busy or locked.
    #[error("run-cost storage is temporarily unavailable")]
    Unavailable,
    /// SQLite returned an unexpected persistence failure whose details were discarded.
    #[error("internal run-cost persistence failure")]
    Internal,
}

impl fmt::Debug for RunCostStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "RunCostStoreError::Unavailable",
            Self::Internal => "RunCostStoreError::Internal",
        })
    }
}

fn map_sqlx_error(error: sqlx::Error) -> RunCostStoreError {
    if sqlite_is_busy_or_locked(&error) {
        RunCostStoreError::Unavailable
    } else {
        RunCostStoreError::Internal
    }
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;
    use time::{macros::datetime, Duration};

    use super::{FinalizedRunCost, PendingRunCost, RunCostStore, RunFinalization};
    use crate::{
        domain::{merge_queue::PullRequestNumber, repository::RepositoryId},
        storage::open_database,
        telemetry::{
            run_cost::PullRequestResolution,
            trace::CommitSha,
            workflow::{
                DisplayName, WorkflowConclusion, WorkflowEvent, WorkflowRunAttempt, WorkflowRunId,
            },
        },
    };

    const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    async fn store_with_repository() -> (tempfile::TempDir, SqlitePool, RunCostStore, RepositoryId)
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
        let store = RunCostStore::new(pool.clone());
        (
            directory,
            pool,
            store,
            RepositoryId::new(id).expect("repository id is positive"),
        )
    }

    fn pending(run_attempt: i64) -> PendingRunCost {
        PendingRunCost {
            run_id: WorkflowRunId::new(31).expect("run id is positive"),
            run_attempt: WorkflowRunAttempt::new(run_attempt).expect("attempt is positive"),
            event: WorkflowEvent::PullRequest,
            workflow_name: DisplayName::sanitize("CI"),
            workflow_file: None,
            conclusion: WorkflowConclusion::Cancelled,
            head_sha: CommitSha::parse(HEAD_SHA).ok(),
            pull_request: Some((
                PullRequestNumber::new(42).expect("PR is positive"),
                PullRequestResolution::PullRequests,
            )),
        }
    }

    fn finalized(pending: &PendingRunCost, total_usd: f64) -> FinalizedRunCost {
        FinalizedRunCost {
            pull_request: pending.pull_request.map(|(number, _)| number),
            resolution: PullRequestResolution::PullRequests,
            total_usd,
            price_table_version: Some("0123456789ab".to_owned()),
            event: pending.event,
            head_sha: pending.head_sha.clone(),
            cost_by_source: vec![("github_actions".to_owned(), total_usd)],
        }
    }

    #[tokio::test]
    async fn an_attempt_is_scheduled_once_and_due_only_after_its_due_time() {
        let (_directory, _pool, store, repository_id) = store_with_repository().await;
        let due_at = datetime!(2026-11-18 10:05:00 UTC);

        assert!(store
            .schedule(repository_id, &pending(1), due_at)
            .await
            .expect("schedule succeeds"));
        assert!(!store
            .schedule(repository_id, &pending(1), due_at + Duration::hours(1))
            .await
            .expect("a second schedule is a no-op"));

        assert!(store
            .due(due_at - Duration::milliseconds(1), 10)
            .await
            .expect("due query succeeds")
            .runs
            .is_empty());
        let due = store
            .due(due_at, 10)
            .await
            .expect("due query succeeds")
            .runs;
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].repository_name.as_str(), "owner/repo");
        assert_eq!(due[0].pending, pending(1));
    }

    #[tokio::test]
    async fn finalizing_is_once_and_writes_the_pull_request_total() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;
        let due_at = datetime!(2026-11-18 10:05:00 UTC);
        store
            .schedule(repository_id, &pending(1), due_at)
            .await
            .expect("schedule succeeds");
        store
            .schedule(repository_id, &pending(2), due_at)
            .await
            .expect("schedule succeeds");
        let run_id = pending(1).run_id;

        assert!(!store
            .is_finalized(repository_id, run_id, pending(1).run_attempt)
            .await
            .expect("check succeeds"));
        assert!(store
            .finalize_pending(repository_id, &pending(1), finalized(&pending(1), 0.5))
            .await
            .expect("finalize succeeds"));
        assert!(!store
            .finalize_pending(repository_id, &pending(1), finalized(&pending(1), 9.0))
            .await
            .expect("a second finalize is a no-op"));

        assert!(store
            .is_finalized(repository_id, run_id, pending(1).run_attempt)
            .await
            .expect("check succeeds"));
        assert!(!store
            .is_finalized(repository_id, run_id, pending(2).run_attempt)
            .await
            .expect("check succeeds"));
        let due = store
            .due(due_at, 10)
            .await
            .expect("due query succeeds")
            .runs;
        assert_eq!(due.len(), 1, "only the second attempt is still pending");
        let stored: Vec<(i64, i64, f64, Option<String>)> = sqlx::query_as(
            "SELECT pull_request_number, workflow_run_attempt, total_usd, price_table_version \
             FROM pull_request_run_costs",
        )
        .fetch_all(&pool)
        .await
        .expect("totals load");
        assert_eq!(stored, [(42, 1, 0.5, Some("0123456789ab".to_owned()))]);
    }

    #[tokio::test]
    async fn a_failed_attempt_is_deferred_with_a_bounded_doubling_delay() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;
        let due_at = datetime!(2026-11-18 10:05:00 UTC);
        store
            .schedule(repository_id, &pending(1), due_at)
            .await
            .expect("schedule succeeds");
        store
            .schedule(repository_id, &pending(2), due_at + Duration::seconds(1))
            .await
            .expect("schedule succeeds");
        let first = pending(1);

        let mut delays = Vec::new();
        for _ in 0..9 {
            store
                .defer(repository_id, first.run_id, first.run_attempt, due_at)
                .await
                .expect("defer succeeds");
            let deferred_to: String = sqlx::query_scalar(
                "SELECT due_at FROM run_cost_finalizations WHERE workflow_run_attempt = 1",
            )
            .fetch_one(&pool)
            .await
            .expect("due time loads");
            let deferred_to = time::PrimitiveDateTime::parse(
                deferred_to.trim_end_matches('Z'),
                &time::format_description::well_known::Iso8601::DEFAULT,
            )
            .expect("due time parses")
            .assume_utc();
            delays.push((deferred_to - due_at).whole_seconds());
        }
        assert_eq!(delays, [30, 60, 120, 240, 480, 960, 1920, 3600, 3600]);

        let due = store
            .due(due_at + Duration::seconds(30), 10)
            .await
            .expect("due query succeeds")
            .runs;
        assert_eq!(
            due.iter()
                .map(|run| run.pending.run_attempt.get())
                .collect::<Vec<_>>(),
            [2],
            "a deferred attempt no longer crowds out the next one"
        );
        let failed_attempts: i64 = sqlx::query_scalar(
            "SELECT failed_attempts FROM run_cost_finalizations WHERE workflow_run_attempt = 1",
        )
        .fetch_one(&pool)
        .await
        .expect("attempt count loads");
        assert_eq!(failed_attempts, 9);
    }

    #[tokio::test]
    async fn an_unreadable_row_is_abandoned_and_never_polled_again() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;
        let due_at = datetime!(2026-11-18 10:05:00 UTC);
        for attempt in [1, 2] {
            store
                .schedule(repository_id, &pending(attempt), due_at)
                .await
                .expect("schedule succeeds");
        }
        let mut connection = pool.acquire().await.expect("connection is available");
        sqlx::query("PRAGMA ignore_check_constraints = ON")
            .execute(&mut *connection)
            .await
            .expect("check constraints can be relaxed");
        sqlx::query(
            "UPDATE run_cost_finalizations SET conclusion = 'unknown' \
             WHERE workflow_run_attempt = 1",
        )
        .execute(&mut *connection)
        .await
        .expect("fixture corrupts one row");
        sqlx::query("PRAGMA ignore_check_constraints = OFF")
            .execute(&mut *connection)
            .await
            .expect("check constraints are restored");
        drop(connection);

        sqlx::query(
            "CREATE TRIGGER refuse_abandon BEFORE UPDATE OF abandoned_at \
             ON run_cost_finalizations BEGIN SELECT RAISE(ABORT, 'refused'); END",
        )
        .execute(&pool)
        .await
        .expect("trigger installs");
        let due = store
            .due(due_at, 10)
            .await
            .expect("a failed abandon does not fail the poll");
        assert_eq!((due.abandoned, due.abandon_failed), (0, 1));
        assert_eq!(due.runs.len(), 1, "valid rows are still returned");
        sqlx::query("DROP TRIGGER refuse_abandon")
            .execute(&pool)
            .await
            .expect("trigger drops");

        let due = store.due(due_at, 10).await.expect("due query succeeds");
        assert_eq!((due.abandoned, due.abandon_failed), (1, 0));
        assert_eq!(due.runs.len(), 1);
        assert_eq!(due.runs[0].pending, pending(2));

        let due = store.due(due_at, 10).await.expect("due query succeeds");
        assert_eq!(due.abandoned, 0, "an abandoned row is not read again");
        assert_eq!(due.runs.len(), 1);
        assert!(!store
            .is_finalized(repository_id, pending(1).run_id, pending(1).run_attempt)
            .await
            .expect("check succeeds"));
    }

    #[tokio::test]
    async fn a_rescheduled_attempt_replaces_a_surviving_pull_request_total() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;
        let due_at = datetime!(2026-11-18 10:05:00 UTC);
        store
            .schedule(repository_id, &pending(1), due_at)
            .await
            .expect("schedule succeeds");
        store
            .finalize_pending(repository_id, &pending(1), finalized(&pending(1), 0.5))
            .await
            .expect("finalize succeeds");
        sqlx::query("DELETE FROM run_cost_finalizations")
            .execute(&pool)
            .await
            .expect("finalization row is pruned");

        store
            .schedule(repository_id, &pending(1), due_at)
            .await
            .expect("the attempt is scheduled again");
        assert!(store
            .finalize_pending(repository_id, &pending(1), finalized(&pending(1), 0.75))
            .await
            .expect("finalizing over a surviving total succeeds"));
        let stored: Vec<(i64, f64)> =
            sqlx::query_as("SELECT workflow_run_attempt, total_usd FROM pull_request_run_costs")
                .fetch_all(&pool)
                .await
                .expect("totals load");
        assert_eq!(
            stored,
            [(1, 0.75)],
            "the pull-request total matches the finalization"
        );

        sqlx::query("DELETE FROM run_cost_finalizations")
            .execute(&pool)
            .await
            .expect("finalization row is pruned again");
        store
            .schedule(repository_id, &pending(1), due_at)
            .await
            .expect("the attempt is scheduled a third time");
        let other_pull_request = FinalizedRunCost {
            pull_request: Some(PullRequestNumber::new(99).expect("pull request is positive")),
            ..finalized(&pending(1), 0.25)
        };
        assert!(store
            .finalize_pending(repository_id, &pending(1), other_pull_request)
            .await
            .expect("finalizing under another pull request succeeds"));
        let stored: Vec<(i64, f64)> =
            sqlx::query_as("SELECT pull_request_number, total_usd FROM pull_request_run_costs")
                .fetch_all(&pool)
                .await
                .expect("totals load");
        assert_eq!(
            stored,
            [(99, 0.25)],
            "the attempt counts under one pull request only"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_job_written_while_finalizing_waits_and_then_sees_the_attempt_final() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;
        let due_at = datetime!(2026-11-18 10:05:00 UTC);
        store
            .schedule(repository_id, &pending(1), due_at)
            .await
            .expect("schedule succeeds");
        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let finalizing = tokio::spawn({
            let store = store.clone();
            async move {
                store
                    .finalize(
                        repository_id,
                        pending(1).run_id,
                        pending(1).run_attempt,
                        move |jobs, _| {
                            locked_tx.send(()).expect("test waits for the lock");
                            release_rx.recv().expect("test releases the lock");
                            (finalized(&pending(1), jobs.cost_usd.unwrap_or(0.0)), ())
                        },
                    )
                    .await
            }
        });
        tokio::task::spawn_blocking(move || locked_rx.recv())
            .await
            .expect("lock wait joins")
            .expect("finalize takes the lock");

        let late_job = tokio::spawn({
            let pool = pool.clone();
            let repository_id = repository_id.get();
            async move {
                sqlx::query(
                    "INSERT INTO workflow_job_links (repository_id, workflow_run_id, \
                     workflow_run_attempt, workflow_job_id, conclusion, started_at_nanos, \
                     completed_at_nanos, timing_source, updated_at, billable_minutes, cost_usd, \
                     price_table_version, runner_label) \
                     VALUES (?, ?, ?, 81, 'success', 0, 1, 'reported', \
                     '2026-11-18T10:00:00.000Z', 2, 0.5, '0123456789ab', 'default')",
                )
                .bind(repository_id)
                .bind(pending(1).run_id.get())
                .bind(pending(1).run_attempt.get())
                .execute(&pool)
                .await
            }
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert!(
            !late_job.is_finished(),
            "the job write waits for the finalize transaction"
        );
        release_tx.send(()).expect("finalize is waiting");

        assert_eq!(
            finalizing
                .await
                .expect("finalize joins")
                .expect("finalize succeeds"),
            Some(RunFinalization {
                value: (),
                pull_request_was_final: false,
            })
        );
        late_job
            .await
            .expect("job write joins")
            .expect("job write succeeds after the commit");
        assert!(store
            .is_finalized(repository_id, pending(1).run_id, pending(1).run_attempt)
            .await
            .expect("check succeeds"));
        let total: f64 = sqlx::query_scalar("SELECT total_usd FROM run_cost_finalizations")
            .fetch_one(&pool)
            .await
            .expect("total loads");
        assert_eq!(total, 0.0, "the late job is not in the total");
    }

    #[tokio::test]
    async fn prune_removes_old_finalizations_and_pull_request_totals() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;
        store
            .schedule(
                repository_id,
                &pending(1),
                datetime!(2026-11-18 10:05:00 UTC),
            )
            .await
            .expect("schedule succeeds");
        store
            .finalize_pending(repository_id, &pending(1), finalized(&pending(1), 0.5))
            .await
            .expect("finalize succeeds");
        for table in ["run_cost_finalizations", "pull_request_run_costs"] {
            sqlx::query(&format!(
                "UPDATE {table} SET updated_at = '2026-11-01T00:00:00.000Z'"
            ))
            .execute(&pool)
            .await
            .expect("fixture timestamp updates");
        }

        let cutoff = datetime!(2026-11-02 00:00:00 UTC);
        assert_eq!(store.prune_batch(cutoff).await.expect("prune succeeds"), 1);
        assert_eq!(
            store
                .prune_pull_request_batch(cutoff)
                .await
                .expect("prune succeeds"),
            0,
            "a pending pull request keeps its run totals"
        );
        sqlx::query(
            "UPDATE pull_request_costs SET finalized_at = '2026-11-01T00:00:00.000Z', \
             finalized_by = 'max_age', partial = 0, total_usd = 0.5",
        )
        .execute(&pool)
        .await
        .expect("pull request finalizes");
        assert_eq!(
            store
                .prune_pull_request_batch(cutoff)
                .await
                .expect("prune succeeds"),
            1
        );
        let sources: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM pull_request_run_cost_sources")
            .fetch_one(&pool)
            .await
            .expect("sources count");
        assert_eq!(sources, 0, "the per-source split goes with its total");
        assert_eq!(store.prune_batch(cutoff).await.expect("prune succeeds"), 0);
    }

    #[tokio::test]
    async fn finalizing_records_sources_head_commit_and_pull_request_activity() {
        let (_directory, pool, store, repository_id) = store_with_repository().await;
        let pending = pending(1);
        store
            .schedule(repository_id, &pending, datetime!(2026-11-18 10:05:00 UTC))
            .await
            .expect("schedule succeeds");
        let mut finalized = finalized(&pending, 0.75);
        finalized.cost_by_source = vec![
            ("github_actions".to_owned(), 0.5),
            ("anthropic".to_owned(), 0.25),
        ];

        assert!(store
            .finalize_pending(repository_id, &pending, finalized)
            .await
            .expect("finalize succeeds"));

        let head_sha: Option<String> =
            sqlx::query_scalar("SELECT head_sha FROM pull_request_run_costs")
                .fetch_one(&pool)
                .await
                .expect("run total loads");
        assert_eq!(head_sha.as_deref(), Some(HEAD_SHA));
        let sources: Vec<(String, f64)> = sqlx::query_as(
            "SELECT source, cost_usd FROM pull_request_run_cost_sources ORDER BY source",
        )
        .fetch_all(&pool)
        .await
        .expect("sources load");
        assert_eq!(
            sources,
            [
                ("anthropic".to_owned(), 0.25),
                ("github_actions".to_owned(), 0.5)
            ]
        );
        let (pull_requests, rollups): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM pull_request_costs WHERE pull_request_number = 42), \
             (SELECT COUNT(*) FROM pull_request_cost_rollup)",
        )
        .fetch_one(&pool)
        .await
        .expect("state counts");
        assert_eq!((pull_requests, rollups), (1, 1));
    }

    #[tokio::test]
    async fn unknown_repository_failure_is_redacted() {
        let (_directory, _pool, store, _repository_id) = store_with_repository().await;

        let error = store
            .schedule(
                RepositoryId::new(999).expect("repository id is positive"),
                &pending(1),
                datetime!(2026-11-18 10:05:00 UTC),
            )
            .await
            .expect_err("unknown repository fails");

        assert_eq!(format!("{error:?}"), "RunCostStoreError::Internal");
        assert!(!format!("{error}").contains("FOREIGN KEY"));
    }
}
