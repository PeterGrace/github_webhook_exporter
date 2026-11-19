use std::fmt;

use sqlx::{FromRow, SqliteConnection, SqlitePool};
use thiserror::Error;
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime, UtcOffset};

use crate::{
    domain::{merge_queue::PullRequestNumber, repository::RepositoryId},
    security::CanonicalRepositoryName,
    telemetry::{
        pr_cost::{PullRequestCostTotal, PullRequestFinalizedBy, PullRequestOutcome},
        run_cost::{bounded_breakdown, breakdown_key},
        trace::{self, CommitSha, DatabaseOperation},
    },
};

use super::sqlite_is_busy_or_locked;

const PULL_REQUEST_COST_PRUNE_BATCH_SIZE: i64 = 1_000;

/// The SQL expression naming the first finalization rule that applies to the row `state`, or
/// `NULL`. `?1` to `?4` are the settled, merged, closed and inactive cutoffs, in that order.
///
/// It is a macro rather than a constant so `concat!` can splice it into each query at compile
/// time, keeping `due` and `finalize` on one definition.
macro_rules! finalization_rule {
    () => {
        "CASE \
             WHEN state.outcome = 'merged' AND state.post_merge_activity_at <= ?1 \
                 THEN 'post_merge_run' \
             WHEN state.outcome = 'merged' AND state.post_merge_activity_at IS NULL \
                 AND state.closed_at <= ?2 THEN 'timeout' \
             WHEN state.outcome = 'closed' AND state.closed_at <= ?3 THEN 'closed_grace' \
             WHEN state.last_activity_at <= ?4 THEN 'max_age' \
         END"
    };
}

/// The SQL condition that the row `state` has no run attempt still inside its grace period, or is
/// inactive past `?4`, the maximum-age cutoff.
macro_rules! no_pending_run_unless_inactive {
    () => {
        "(state.last_activity_at <= ?4 OR NOT EXISTS (\
             SELECT 1 FROM run_cost_finalizations AS pending \
             WHERE pending.repository_id = state.repository_id \
             AND pending.finalized_at IS NULL AND pending.abandoned_at IS NULL \
             AND (pending.pull_request_number = state.pull_request_number \
                  OR pending.head_sha = state.merge_commit_sha)\
         ))"
    };
}
const PULL_REQUEST_COST_TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// How a `pull_request` delivery changes the pull request's outcome.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PullRequestCostTransition {
    /// The delivery leaves the outcome unchanged.
    None,
    /// The pull request was closed, merged or not.
    Closed {
        merged: bool,
        merge_commit_sha: Option<CommitSha>,
        closed_at: OffsetDateTime,
    },
    /// A closed pull request was reopened.
    Reopened,
}

/// What one `pull_request` delivery tells the PR cost rollup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PullRequestActivity {
    pub(crate) number: PullRequestNumber,
    /// When GitHub created the pull request, if the payload said.
    pub(crate) created_at: Option<OffsetDateTime>,
    pub(crate) transition: PullRequestCostTransition,
}

/// The times before which each finalization rule applies, fixed for one finalizer poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PullRequestCostCutoffs {
    /// A merged pull request whose latest post-merge run finalized before this is settled.
    pub(crate) settled_before: OffsetDateTime,
    /// A merged pull request closed before this with no post-merge run has timed out.
    pub(crate) merged_before: OffsetDateTime,
    /// A pull request closed without merging before this is past its grace period.
    pub(crate) closed_before: OffsetDateTime,
    /// A pull request with no activity since before this has reached its maximum age.
    pub(crate) inactive_before: OffsetDateTime,
}

/// One pending pull request that a finalization rule now applies to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DuePullRequestCost {
    pub(crate) repository_id: RepositoryId,
    pub(crate) repository_name: CanonicalRepositoryName,
    pub(crate) pull_request: PullRequestNumber,
    pub(crate) finalized_by: PullRequestFinalizedBy,
}

/// Durable per-pull-request cost state, finalized once into one `ci.pr.cost` record.
///
/// The record is built when the pull request is finalized, from the run totals and vendor runs
/// stored against it. Both are keyed by run attempt or delivery, so a redelivered webhook never
/// counts twice.
#[derive(Clone)]
pub(crate) struct PullRequestCostStore {
    pool: SqlitePool,
}

impl PullRequestCostStore {
    /// Creates a pull-request cost store from an already migrated SQLite pool.
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Records one `pull_request` delivery: its creation time, its activity and, until the pull
    /// request is finalized, any close or reopen.
    ///
    /// # Errors
    ///
    /// Returns [`PullRequestCostStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`PullRequestCostStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn record_activity(
        &self,
        repository_id: RepositoryId,
        activity: &PullRequestActivity,
    ) -> Result<(), PullRequestCostStoreError> {
        trace::instrument_database_operation(DatabaseOperation::PullRequestCostRecord, async {
            let (changes_outcome, outcome, merge_commit_sha, closed_at) = match &activity.transition
            {
                PullRequestCostTransition::None => (false, PullRequestOutcome::Open, None, None),
                PullRequestCostTransition::Closed {
                    merged,
                    merge_commit_sha,
                    closed_at,
                } => (
                    true,
                    if *merged {
                        PullRequestOutcome::Merged
                    } else {
                        PullRequestOutcome::Closed
                    },
                    merge_commit_sha.as_ref().map(CommitSha::as_str),
                    Some(format_timestamp(*closed_at)?),
                ),
                PullRequestCostTransition::Reopened => (true, PullRequestOutcome::Open, None, None),
            };
            let created_at = activity.created_at.map(format_timestamp).transpose()?;
            // `excluded` is the row this statement tried to insert, so a conflicting update reads
            // the new values from it. A finalized record keeps its outcome.
            sqlx::query(
                "INSERT INTO pull_request_costs \
                 (repository_id, pull_request_number, created_at, first_activity_at, \
                  last_activity_at, outcome, merge_commit_sha, closed_at) \
                 VALUES (?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
                         strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), ?, ?, ?) \
                 ON CONFLICT(repository_id, pull_request_number) DO UPDATE SET \
                 created_at = coalesce(created_at, excluded.created_at), \
                 last_activity_at = excluded.last_activity_at, \
                 outcome = CASE WHEN ? AND finalized_at IS NULL \
                     THEN excluded.outcome ELSE outcome END, \
                 merge_commit_sha = CASE WHEN ? AND finalized_at IS NULL \
                     THEN excluded.merge_commit_sha ELSE merge_commit_sha END, \
                 closed_at = CASE WHEN ? AND finalized_at IS NULL \
                     THEN excluded.closed_at ELSE closed_at END, \
                 post_merge_activity_at = CASE WHEN ? AND finalized_at IS NULL \
                     THEN NULL ELSE post_merge_activity_at END",
            )
            .bind(repository_id.get())
            .bind(activity.number.get())
            .bind(created_at)
            .bind(outcome.as_str())
            .bind(merge_commit_sha)
            .bind(closed_at)
            .bind(changes_outcome)
            .bind(changes_outcome)
            .bind(changes_outcome)
            .bind(changes_outcome)
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(map_sqlx_error)
        })
        .await
    }

    /// Lists at most `limit` pending pull requests that a finalization rule applies to, least
    /// recently active first.
    ///
    /// A merged or closed pull request waits while one of its runs is still inside its own grace
    /// period. A pull request at its maximum age does not wait, so it is always finalized before
    /// retention prunes it.
    ///
    /// Like [`super::RunCostStore::due`], this runs on every poll, so it records no database span.
    ///
    /// # Errors
    ///
    /// Returns [`PullRequestCostStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`PullRequestCostStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn due(
        &self,
        cutoffs: PullRequestCostCutoffs,
        limit: i64,
    ) -> Result<Vec<DuePullRequestCost>, PullRequestCostStoreError> {
        let [settled_before, merged_before, closed_before, inactive_before] =
            format_cutoffs(cutoffs)?;
        let rows = sqlx::query_as::<_, StoredDuePullRequestCost>(concat!(
            "SELECT due.repository_id, repositories.full_name, due.pull_request_number, \
             due.finalized_by FROM (\
                 SELECT state.repository_id, state.pull_request_number, state.last_activity_at, ",
            finalization_rule!(),
            " AS finalized_by FROM pull_request_costs AS state \
                 WHERE state.finalized_at IS NULL AND ",
            no_pending_run_unless_inactive!(),
            ") AS due \
             JOIN repositories ON repositories.id = due.repository_id \
             WHERE due.finalized_by IS NOT NULL \
             ORDER BY due.last_activity_at LIMIT ?5",
        ))
        .bind(settled_before)
        .bind(merged_before)
        .bind(closed_before)
        .bind(inactive_before)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(rows
            .into_iter()
            .filter_map(StoredDuePullRequestCost::into_due)
            .collect())
    }

    /// Builds one pending pull request's record from its stored run totals and vendor runs, then
    /// marks it final.
    ///
    /// The reads and the update share one transaction that takes SQLite's write lock first, so a
    /// run total or vendor run either commits before the record is built and is counted, or
    /// commits after and sees the pull request as final. The rule `due` chose is evaluated again
    /// under that lock with the same `cutoffs`, so a reopen or a newly pending run that landed in
    /// between leaves the pull request for a later poll.
    ///
    /// # Returns
    ///
    /// The record, or `None` when the pull request was already final or its rule no longer
    /// applies.
    ///
    /// # Errors
    ///
    /// Returns [`PullRequestCostStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`PullRequestCostStoreError::Internal`] for any other persistence failure. On error
    /// nothing is stored and the pull request stays pending.
    pub(crate) async fn finalize(
        &self,
        due: &DuePullRequestCost,
        cutoffs: PullRequestCostCutoffs,
    ) -> Result<Option<PullRequestCostTotal>, PullRequestCostStoreError> {
        let cutoffs = format_cutoffs(cutoffs)?;
        trace::instrument_database_operation(DatabaseOperation::PullRequestCostFinalize, async {
            let mut transaction = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(map_sqlx_error)?;
            let Some(total) = pull_request_total_in(&mut transaction, due, &cutoffs)
                .await
                .map_err(map_sqlx_error)?
            else {
                return Ok(None);
            };
            sqlx::query(
                "UPDATE pull_request_costs SET \
                 finalized_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), finalized_by = ?, \
                 partial = ?, total_usd = ? \
                 WHERE repository_id = ? AND pull_request_number = ? AND finalized_at IS NULL",
            )
            .bind(total.finalized_by.as_str())
            .bind(total.partial)
            .bind(total.total_usd)
            .bind(due.repository_id.get())
            .bind(due.pull_request.get())
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx_error)?;
            transaction.commit().await.map_err(map_sqlx_error)?;
            Ok(Some(total))
        })
        .await
    }

    /// Deletes at most 1,000 finalized pull requests last active before `cutoff`.
    ///
    /// Pending pull requests are never pruned: one inactive for the whole retention period is
    /// finalized with `max_age` first.
    ///
    /// # Errors
    ///
    /// Returns [`PullRequestCostStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`PullRequestCostStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn prune_batch(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<u64, PullRequestCostStoreError> {
        trace::instrument_database_operation(DatabaseOperation::PullRequestCostPrune, async {
            sqlx::query(
                "DELETE FROM pull_request_costs WHERE rowid IN (\
                     SELECT rowid FROM pull_request_costs \
                     WHERE finalized_at IS NOT NULL AND last_activity_at < ? \
                     ORDER BY last_activity_at, rowid LIMIT ?\
                 )",
            )
            .bind(format_timestamp(cutoff)?)
            .bind(PULL_REQUEST_COST_PRUNE_BATCH_SIZE)
            .execute(&self.pool)
            .await
            .map(|result| result.rows_affected())
            .map_err(map_sqlx_error)
        })
        .await
    }
}

/// Records cost activity for one pull request on `connection`, inside the writer's transaction.
///
/// `post_merge_commit` is the head commit of a `push` run; when it is the pull request's merge
/// commit, the run counts as post-merge activity.
///
/// # Returns
///
/// Whether the pull request's record was already final, so the caller can warn that the cost
/// arrived too late to be counted.
pub(crate) async fn record_cost_activity_in(
    connection: &mut SqliteConnection,
    repository_id: RepositoryId,
    pull_request: PullRequestNumber,
    post_merge_commit: Option<&CommitSha>,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "INSERT INTO pull_request_costs \
         (repository_id, pull_request_number, first_activity_at, last_activity_at) \
         VALUES (?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
                 strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) \
         ON CONFLICT(repository_id, pull_request_number) DO UPDATE SET \
         last_activity_at = excluded.last_activity_at, \
         post_merge_activity_at = CASE \
             WHEN finalized_at IS NULL AND outcome = 'merged' AND merge_commit_sha = ? \
             THEN excluded.last_activity_at ELSE post_merge_activity_at END \
         RETURNING finalized_at IS NOT NULL",
    )
    .bind(repository_id.get())
    .bind(pull_request.get())
    .bind(post_merge_commit.map(CommitSha::as_str))
    .fetch_one(&mut *connection)
    .await
}

/// Reads one pending pull request's state and sums its stored costs, or `None` when it is
/// already final or the rule `due` chose no longer applies at `cutoffs`.
async fn pull_request_total_in(
    connection: &mut SqliteConnection,
    due: &DuePullRequestCost,
    cutoffs: &[String; 4],
) -> Result<Option<PullRequestCostTotal>, sqlx::Error> {
    let repository_id = due.repository_id.get();
    let pull_request = due.pull_request.get();
    let [settled_before, merged_before, closed_before, inactive_before] = cutoffs;
    // `julianday` reads the stored UTC timestamps, so the difference is in days.
    let state = sqlx::query_as::<_, StoredPullRequestState>(concat!(
        "SELECT state.outcome, ",
        finalization_rule!(),
        " AS finalized_by, \
         (state.created_at IS NULL OR rollup.started_at IS NULL \
          OR state.created_at < rollup.started_at) AS partial, \
         (julianday(coalesce(state.closed_at, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))) \
          - julianday(coalesce(state.created_at, state.first_activity_at))) * 24 AS open_hours \
         FROM pull_request_costs AS state \
         LEFT JOIN pull_request_cost_rollup AS rollup ON rollup.id = 1 \
         WHERE state.repository_id = ?5 AND state.pull_request_number = ?6 \
         AND state.finalized_at IS NULL AND ",
        no_pending_run_unless_inactive!(),
    ))
    .bind(settled_before)
    .bind(merged_before)
    .bind(closed_before)
    .bind(inactive_before)
    .bind(repository_id)
    .bind(pull_request)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(state) =
        state.filter(|state| state.finalized_by.as_deref() == Some(due.finalized_by.as_str()))
    else {
        return Ok(None);
    };
    let Some(outcome) = PullRequestOutcome::from_stored(&state.outcome) else {
        return Err(sqlx::Error::Decode("unknown pull request outcome".into()));
    };
    let (attempts, runs, run_total_usd): (i64, i64, f64) = sqlx::query_as(
        "SELECT COUNT(*), COUNT(DISTINCT workflow_run_id), TOTAL(total_usd) \
         FROM pull_request_run_costs WHERE repository_id = ? AND pull_request_number = ?",
    )
    .bind(repository_id)
    .bind(pull_request)
    .fetch_one(&mut *connection)
    .await?;
    let run_sources: Vec<(String, f64)> = sqlx::query_as(
        "SELECT source, TOTAL(cost_usd) FROM pull_request_run_cost_sources \
         WHERE repository_id = ? AND pull_request_number = ? GROUP BY source",
    )
    .bind(repository_id)
    .bind(pull_request)
    .fetch_all(&mut *connection)
    .await?;
    let vendors: Vec<(String, i64, f64)> = sqlx::query_as(
        "SELECT vendor, COUNT(*), TOTAL(cost_usd) FROM ci_vendor_runs \
         WHERE repository_id = ? AND pull_request_number = ? GROUP BY vendor",
    )
    .bind(repository_id)
    .bind(pull_request)
    .fetch_all(&mut *connection)
    .await?;
    let commits: i64 = sqlx::query_scalar(
        "SELECT COUNT(DISTINCT head_sha) FROM (\
             SELECT head_sha FROM pull_request_run_costs \
             WHERE repository_id = ?1 AND pull_request_number = ?2 \
             UNION ALL \
             SELECT head_sha FROM ci_vendor_runs \
             WHERE repository_id = ?1 AND pull_request_number = ?2\
         )",
    )
    .bind(repository_id)
    .bind(pull_request)
    .fetch_one(&mut *connection)
    .await?;
    let vendor_runs = vendors.iter().map(|(_, runs, _)| runs).sum();
    let vendor_total_usd = vendors.iter().map(|(_, _, cost_usd)| cost_usd).sum::<f64>();
    let cost_by_source = bounded_breakdown(
        run_sources
            .into_iter()
            .chain(
                vendors
                    .into_iter()
                    .map(|(vendor, _, cost_usd)| (vendor, cost_usd)),
            )
            .filter_map(|(source, cost_usd)| Some((breakdown_key(&source)?, cost_usd))),
    );
    Ok(Some(PullRequestCostTotal {
        repository_name: due.repository_name.clone(),
        pull_request: due.pull_request,
        outcome,
        finalized_by: due.finalized_by,
        partial: state.partial,
        commits,
        runs,
        reruns: attempts - runs,
        vendor_runs,
        open_hours: state.open_hours.max(0.0),
        total_usd: run_total_usd + vendor_total_usd,
        cost_by_source,
        finalized_at: std::time::SystemTime::now(),
    }))
}

#[derive(FromRow)]
struct StoredDuePullRequestCost {
    repository_id: i64,
    full_name: String,
    pull_request_number: i64,
    finalized_by: String,
}

impl StoredDuePullRequestCost {
    fn into_due(self) -> Option<DuePullRequestCost> {
        Some(DuePullRequestCost {
            repository_id: RepositoryId::new(self.repository_id)?,
            repository_name: CanonicalRepositoryName::new(&self.full_name).ok()?,
            pull_request: PullRequestNumber::new(self.pull_request_number).ok()?,
            finalized_by: PullRequestFinalizedBy::from_stored(&self.finalized_by)?,
        })
    }
}

#[derive(FromRow)]
struct StoredPullRequestState {
    outcome: String,
    finalized_by: Option<String>,
    partial: bool,
    open_hours: f64,
}

/// A stable, redacted pull-request cost persistence failure.
#[derive(Clone, Copy, Error, PartialEq, Eq)]
pub(crate) enum PullRequestCostStoreError {
    /// SQLite is temporarily busy or locked.
    #[error("pull-request cost storage is temporarily unavailable")]
    Unavailable,
    /// SQLite returned an unexpected persistence failure whose details were discarded.
    #[error("internal pull-request cost persistence failure")]
    Internal,
}

impl fmt::Debug for PullRequestCostStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "PullRequestCostStoreError::Unavailable",
            Self::Internal => "PullRequestCostStoreError::Internal",
        })
    }
}

/// Formats the four cutoffs in the `?1` to `?4` order the rule fragments use.
fn format_cutoffs(
    cutoffs: PullRequestCostCutoffs,
) -> Result<[String; 4], PullRequestCostStoreError> {
    Ok([
        format_timestamp(cutoffs.settled_before)?,
        format_timestamp(cutoffs.merged_before)?,
        format_timestamp(cutoffs.closed_before)?,
        format_timestamp(cutoffs.inactive_before)?,
    ])
}

fn format_timestamp(value: OffsetDateTime) -> Result<String, PullRequestCostStoreError> {
    value
        .to_offset(UtcOffset::UTC)
        .format(PULL_REQUEST_COST_TIMESTAMP_FORMAT)
        .map_err(|_| PullRequestCostStoreError::Internal)
}

fn map_sqlx_error(error: sqlx::Error) -> PullRequestCostStoreError {
    if sqlite_is_busy_or_locked(&error) {
        PullRequestCostStoreError::Unavailable
    } else {
        PullRequestCostStoreError::Internal
    }
}

#[cfg(test)]
mod tests {
    use sqlx::SqlitePool;
    use time::{macros::datetime, Duration, OffsetDateTime};

    use super::{
        PullRequestActivity, PullRequestCostCutoffs, PullRequestCostStore,
        PullRequestCostTransition,
    };
    use crate::{
        domain::{merge_queue::PullRequestNumber, repository::RepositoryId},
        storage::{open_database, FinalizedRunCost, PendingRunCost, RunCostStore},
        telemetry::{
            pr_cost::{PullRequestFinalizedBy, PullRequestOutcome},
            run_cost::PullRequestResolution,
            trace::CommitSha,
            workflow::{WorkflowConclusion, WorkflowEvent, WorkflowRunAttempt, WorkflowRunId},
        },
    };

    const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const MERGE_SHA: &str = "1123456789abcdef0123456789abcdef01234567";
    const PULL_REQUEST: i64 = 42;

    struct Fixture {
        _directory: tempfile::TempDir,
        pool: SqlitePool,
        store: PullRequestCostStore,
        runs: RunCostStore,
        repository_id: RepositoryId,
    }

    async fn fixture() -> Fixture {
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
        Fixture {
            _directory: directory,
            store: PullRequestCostStore::new(pool.clone()),
            runs: RunCostStore::new(pool.clone()),
            pool,
            repository_id: RepositoryId::new(id).expect("repository id is positive"),
        }
    }

    fn number() -> PullRequestNumber {
        PullRequestNumber::new(PULL_REQUEST).expect("PR is positive")
    }

    fn sha(value: &str) -> CommitSha {
        CommitSha::parse(value).expect("SHA parses")
    }

    fn pending(run_id: i64, run_attempt: i64, event: WorkflowEvent, head: &str) -> PendingRunCost {
        PendingRunCost {
            run_id: WorkflowRunId::new(run_id).expect("run id is positive"),
            run_attempt: WorkflowRunAttempt::new(run_attempt).expect("attempt is positive"),
            event,
            workflow_name: None,
            workflow_file: None,
            conclusion: WorkflowConclusion::Success,
            head_sha: Some(sha(head)),
            pull_request: Some((number(), PullRequestResolution::PullRequests)),
        }
    }

    impl Fixture {
        /// Schedules and finalizes one run attempt attributed to the pull request.
        async fn finalize_run(&self, pending: &PendingRunCost, sources: &[(&str, f64)]) {
            self.runs
                .schedule(self.repository_id, pending, datetime!(2026-11-18 10:05 UTC))
                .await
                .expect("schedule succeeds");
            let finalized = FinalizedRunCost {
                pull_request: Some(number()),
                resolution: PullRequestResolution::PullRequests,
                total_usd: sources.iter().map(|(_, cost_usd)| cost_usd).sum(),
                price_table_version: None,
                event: pending.event,
                head_sha: pending.head_sha.clone(),
                cost_by_source: sources
                    .iter()
                    .map(|(source, cost_usd)| ((*source).to_owned(), *cost_usd))
                    .collect(),
            };
            assert!(self
                .runs
                .finalize_pending(self.repository_id, pending, finalized)
                .await
                .expect("finalize succeeds"));
        }

        async fn record(&self, created_at: OffsetDateTime, transition: PullRequestCostTransition) {
            self.store
                .record_activity(
                    self.repository_id,
                    &PullRequestActivity {
                        number: number(),
                        created_at: Some(created_at),
                        transition,
                    },
                )
                .await
                .expect("activity records");
        }

        async fn due_rules(&self, cutoffs: PullRequestCostCutoffs) -> Vec<PullRequestFinalizedBy> {
            self.store
                .due(cutoffs, 10)
                .await
                .expect("due query succeeds")
                .into_iter()
                .map(|due| due.finalized_by)
                .collect()
        }
    }

    fn closed(merged: bool) -> PullRequestCostTransition {
        PullRequestCostTransition::Closed {
            merged,
            merge_commit_sha: merged.then(|| sha(MERGE_SHA)),
            closed_at: OffsetDateTime::now_utc(),
        }
    }

    /// Cutoffs where no rule has elapsed yet.
    fn nothing_elapsed() -> PullRequestCostCutoffs {
        let long_ago = OffsetDateTime::now_utc() - Duration::days(1);
        PullRequestCostCutoffs {
            settled_before: long_ago,
            merged_before: long_ago,
            closed_before: long_ago,
            inactive_before: long_ago,
        }
    }

    fn elapsed(rule: PullRequestFinalizedBy) -> PullRequestCostCutoffs {
        let later = OffsetDateTime::now_utc() + Duration::hours(1);
        let mut cutoffs = nothing_elapsed();
        match rule {
            PullRequestFinalizedBy::PostMergeRun => cutoffs.settled_before = later,
            PullRequestFinalizedBy::Timeout => cutoffs.merged_before = later,
            PullRequestFinalizedBy::ClosedGrace => cutoffs.closed_before = later,
            PullRequestFinalizedBy::MaxAge => cutoffs.inactive_before = later,
        }
        cutoffs
    }

    #[tokio::test]
    async fn a_merged_pull_request_finalizes_once_after_its_post_merge_runs_settle() {
        let fixture = fixture().await;
        fixture
            .finalize_run(
                &pending(1, 1, WorkflowEvent::PullRequest, HEAD_SHA),
                &[("github_actions", 1.0), ("anthropic", 0.25)],
            )
            .await;
        fixture
            .finalize_run(
                &pending(1, 2, WorkflowEvent::PullRequest, HEAD_SHA),
                &[("github_actions", 1.0)],
            )
            .await;
        fixture
            .record(datetime!(2030-01-01 00:00 UTC), closed(true))
            .await;
        assert!(
            fixture
                .due_rules(elapsed(PullRequestFinalizedBy::PostMergeRun))
                .await
                .is_empty(),
            "a merged pull request without a post-merge run is not settled"
        );
        fixture
            .finalize_run(
                &pending(2, 1, WorkflowEvent::Push, MERGE_SHA),
                &[("github_actions", 0.5)],
            )
            .await;
        assert!(fixture.due_rules(nothing_elapsed()).await.is_empty());

        let due = fixture
            .store
            .due(elapsed(PullRequestFinalizedBy::PostMergeRun), 10)
            .await
            .expect("due query succeeds");
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].finalized_by, PullRequestFinalizedBy::PostMergeRun);
        let total = fixture
            .store
            .finalize(&due[0], elapsed(due[0].finalized_by))
            .await
            .expect("finalize succeeds")
            .expect("the pull request was pending");

        assert_eq!(total.outcome, PullRequestOutcome::Merged);
        assert_eq!((total.commits, total.runs, total.reruns), (2, 2, 1));
        assert_eq!(total.vendor_runs, 0);
        assert!((total.total_usd - 2.75).abs() < 1e-9);
        assert_eq!(
            total.cost_by_source,
            [
                ("anthropic".to_owned(), 0.25),
                ("github_actions".to_owned(), 2.5)
            ]
        );
        assert!(
            !total.partial,
            "created after run totals began, so the record is complete"
        );
        assert!(fixture
            .store
            .finalize(&due[0], elapsed(due[0].finalized_by))
            .await
            .expect("finalize succeeds")
            .is_none());
        assert!(fixture
            .due_rules(elapsed(PullRequestFinalizedBy::MaxAge))
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn redelivered_runs_are_counted_once() {
        let fixture = fixture().await;
        let run = pending(1, 1, WorkflowEvent::PullRequest, HEAD_SHA);
        fixture.finalize_run(&run, &[("github_actions", 1.0)]).await;
        assert!(!fixture
            .runs
            .schedule(fixture.repository_id, &run, datetime!(2026-11-18 10:05 UTC))
            .await
            .expect("a redelivered schedule is a no-op"));
        fixture
            .record(datetime!(2030-01-01 00:00 UTC), closed(false))
            .await;

        let due = fixture
            .store
            .due(elapsed(PullRequestFinalizedBy::ClosedGrace), 10)
            .await
            .expect("due query succeeds");
        let total = fixture
            .store
            .finalize(&due[0], elapsed(due[0].finalized_by))
            .await
            .expect("finalize succeeds")
            .expect("the pull request was pending");

        assert_eq!(total.finalized_by, PullRequestFinalizedBy::ClosedGrace);
        assert_eq!(total.outcome, PullRequestOutcome::Closed);
        assert_eq!((total.runs, total.reruns), (1, 0));
        assert!((total.total_usd - 1.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn each_rule_applies_only_to_its_outcome() {
        let fixture = fixture().await;
        fixture
            .record(
                datetime!(2030-01-01 00:00 UTC),
                PullRequestCostTransition::None,
            )
            .await;
        for rule in [
            PullRequestFinalizedBy::PostMergeRun,
            PullRequestFinalizedBy::Timeout,
            PullRequestFinalizedBy::ClosedGrace,
        ] {
            assert!(
                fixture.due_rules(elapsed(rule)).await.is_empty(),
                "{rule:?}"
            );
        }
        fixture
            .record(datetime!(2030-01-01 00:00 UTC), closed(true))
            .await;
        assert_eq!(
            fixture
                .due_rules(elapsed(PullRequestFinalizedBy::Timeout))
                .await,
            [PullRequestFinalizedBy::Timeout]
        );
        assert!(fixture
            .due_rules(elapsed(PullRequestFinalizedBy::ClosedGrace))
            .await
            .is_empty());
        fixture
            .record(
                datetime!(2030-01-01 00:00 UTC),
                PullRequestCostTransition::Reopened,
            )
            .await;
        assert!(
            fixture
                .due_rules(elapsed(PullRequestFinalizedBy::Timeout))
                .await
                .is_empty(),
            "a reopened pull request is open again"
        );
        assert_eq!(
            fixture
                .due_rules(elapsed(PullRequestFinalizedBy::MaxAge))
                .await,
            [PullRequestFinalizedBy::MaxAge]
        );
    }

    #[tokio::test]
    async fn a_pending_run_holds_a_closed_pull_request_but_not_one_at_max_age() {
        let fixture = fixture().await;
        fixture
            .runs
            .schedule(
                fixture.repository_id,
                &pending(1, 1, WorkflowEvent::PullRequest, HEAD_SHA),
                datetime!(2026-11-18 10:05 UTC),
            )
            .await
            .expect("schedule succeeds");
        fixture
            .record(datetime!(2030-01-01 00:00 UTC), closed(false))
            .await;

        assert!(fixture
            .due_rules(elapsed(PullRequestFinalizedBy::ClosedGrace))
            .await
            .is_empty());
        let mut cutoffs = elapsed(PullRequestFinalizedBy::ClosedGrace);
        cutoffs.inactive_before = elapsed(PullRequestFinalizedBy::MaxAge).inactive_before;
        assert_eq!(
            fixture.due_rules(cutoffs).await,
            [PullRequestFinalizedBy::ClosedGrace],
            "at max age the pull request no longer waits, and its outcome rule still names it"
        );
    }

    #[tokio::test]
    async fn a_reopen_or_new_pending_run_after_the_due_poll_leaves_the_pull_request_pending() {
        let fixture = fixture().await;
        let cutoffs = elapsed(PullRequestFinalizedBy::ClosedGrace);
        fixture
            .record(datetime!(2030-01-01 00:00 UTC), closed(false))
            .await;
        let due = fixture
            .store
            .due(cutoffs, 10)
            .await
            .expect("due query succeeds");
        fixture
            .runs
            .schedule(
                fixture.repository_id,
                &pending(1, 1, WorkflowEvent::PullRequest, HEAD_SHA),
                datetime!(2026-11-18 10:05 UTC),
            )
            .await
            .expect("schedule succeeds");

        assert!(
            fixture
                .store
                .finalize(&due[0], cutoffs)
                .await
                .expect("finalize succeeds")
                .is_none(),
            "a run scheduled after the due poll is waited for"
        );

        sqlx::query("DELETE FROM run_cost_finalizations")
            .execute(&fixture.pool)
            .await
            .expect("pending run clears");
        let due = fixture
            .store
            .due(cutoffs, 10)
            .await
            .expect("due query succeeds");
        fixture
            .record(
                datetime!(2030-01-01 00:00 UTC),
                PullRequestCostTransition::Reopened,
            )
            .await;

        assert!(
            fixture
                .store
                .finalize(&due[0], cutoffs)
                .await
                .expect("finalize succeeds")
                .is_none(),
            "a pull request reopened after the due poll is no longer past its closed grace"
        );
        let pending: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pull_request_costs WHERE finalized_at IS NULL",
        )
        .fetch_one(&fixture.pool)
        .await
        .expect("state loads");
        assert_eq!(pending, 1);
    }

    #[tokio::test]
    async fn a_pull_request_created_before_run_totals_began_is_partial() {
        let fixture = fixture().await;
        fixture
            .finalize_run(
                &pending(1, 1, WorkflowEvent::PullRequest, HEAD_SHA),
                &[("github_actions", 1.0)],
            )
            .await;
        fixture
            .record(datetime!(2020-01-01 00:00 UTC), closed(false))
            .await;
        let due = fixture
            .store
            .due(elapsed(PullRequestFinalizedBy::ClosedGrace), 10)
            .await
            .expect("due query succeeds");

        let total = fixture
            .store
            .finalize(&due[0], elapsed(due[0].finalized_by))
            .await
            .expect("finalize succeeds")
            .expect("the pull request was pending");

        assert!(total.partial);
        assert!(total.open_hours > 24.0 * 365.0);
    }

    #[tokio::test]
    async fn the_migration_splits_existing_run_totals_by_source_and_dates_the_rollup() {
        const PULL_REQUEST_COST_VERSION: i64 = 202_611_200_001;
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
            .filter(|migration| migration.version < PULL_REQUEST_COST_VERSION)
            .cloned()
            .collect::<Vec<_>>()
            .into();
        earlier.run(&pool).await.expect("earlier migrations apply");
        for statement in [
            "INSERT INTO repositories (id, full_name, webhook_secret_ciphertext, \
             webhook_secret_nonce, encryption_version, enabled, created_at, updated_at) \
             VALUES (1, 'owner/repo', X'01', X'02', 1, 1, \
             '2026-11-18T10:00:00.000Z', '2026-11-18T10:00:00.000Z')",
            "INSERT INTO run_cost_finalizations (repository_id, workflow_run_id, \
             workflow_run_attempt, event, conclusion, head_sha, pull_request_number, \
             pull_request_resolution, due_at, finalized_at, total_usd, updated_at) \
             VALUES (1, 31, 1, 'pull_request', 'success', \
             '0123456789abcdef0123456789abcdef01234567', 42, 'pull_requests', \
             '2026-11-18T10:05:00.000Z', '2026-11-18T10:05:00.000Z', 0.75, \
             '2026-11-18T10:05:00.000Z')",
            "INSERT INTO pull_request_run_costs (repository_id, pull_request_number, \
             workflow_run_id, workflow_run_attempt, total_usd, updated_at) \
             VALUES (1, 42, 31, 1, 0.75, '2026-11-18T10:05:00.000Z')",
            "INSERT INTO workflow_job_links (repository_id, workflow_run_id, \
             workflow_run_attempt, workflow_job_id, conclusion, started_at_nanos, \
             completed_at_nanos, timing_source, updated_at, billable_minutes, cost_usd) \
             VALUES (1, 31, 1, 7, 'success', 0, 1, 'reported', \
             '2026-11-18T10:00:00.000Z', 2, 0.5)",
            "INSERT INTO ci_usage_entries (repository_id, workflow_run_id, \
             workflow_run_attempt, source, cost_usd, price_fell_back, updated_at) \
             VALUES (1, 31, 1, 'anthropic', 0.25, 0, '2026-11-18T10:00:00.000Z')",
            "INSERT INTO ci_vendor_runs (repository_id, pull_request_number, vendor, trigger, \
             cost_usd, price_table_version, updated_at) \
             VALUES (1, 43, 'review-bot', 'check_suite', 1, '0123456789ab', \
             '2026-11-18T11:00:00.000Z')",
        ] {
            sqlx::query(statement)
                .execute(&pool)
                .await
                .expect("pre-migration row inserts");
        }
        pool.close().await;

        let migrated = open_database(&path)
            .await
            .expect("remaining migrations apply");

        let sources: Vec<(String, f64)> = sqlx::query_as(
            "SELECT source, cost_usd FROM pull_request_run_cost_sources ORDER BY source",
        )
        .fetch_all(&migrated)
        .await
        .expect("sources load");
        assert_eq!(
            sources,
            [
                ("anthropic".to_owned(), 0.25),
                ("github_actions".to_owned(), 0.5)
            ]
        );
        let head_sha: Option<String> =
            sqlx::query_scalar("SELECT head_sha FROM pull_request_run_costs")
                .fetch_one(&migrated)
                .await
                .expect("run total loads");
        assert_eq!(head_sha.as_deref(), Some(HEAD_SHA));
        let started_at: String =
            sqlx::query_scalar("SELECT started_at FROM pull_request_cost_rollup")
                .fetch_one(&migrated)
                .await
                .expect("rollup start loads");
        assert_eq!(started_at, "2026-11-18T10:05:00.000Z");
        let seeded: Vec<(i64, String, Option<String>, Option<String>)> = sqlx::query_as(
            "SELECT pull_request_number, outcome, created_at, finalized_at \
             FROM pull_request_costs ORDER BY pull_request_number",
        )
        .fetch_all(&migrated)
        .await
        .expect("seeded state loads");
        assert_eq!(
            seeded,
            [
                (42, "open".to_owned(), None, None),
                (43, "open".to_owned(), None, None)
            ],
            "pull requests with stored run totals or vendor runs start pending"
        );
    }

    #[tokio::test]
    async fn only_finalized_pull_requests_are_pruned() {
        let fixture = fixture().await;
        fixture
            .record(datetime!(2030-01-01 00:00 UTC), closed(false))
            .await;
        sqlx::query("UPDATE pull_request_costs SET last_activity_at = '2026-01-01T00:00:00.000Z'")
            .execute(&fixture.pool)
            .await
            .expect("state ages");
        let cutoff = datetime!(2026-06-01 00:00 UTC);

        assert_eq!(
            fixture
                .store
                .prune_batch(cutoff)
                .await
                .expect("prune succeeds"),
            0,
            "a pending pull request is finalized with max_age before it is pruned"
        );
        let due = fixture
            .store
            .due(elapsed(PullRequestFinalizedBy::MaxAge), 10)
            .await
            .expect("due query succeeds");
        fixture
            .store
            .finalize(&due[0], elapsed(due[0].finalized_by))
            .await
            .expect("finalize succeeds")
            .expect("the pull request was pending");
        assert_eq!(
            fixture
                .store
                .prune_batch(cutoff)
                .await
                .expect("prune succeeds"),
            1
        );
    }
}
