use std::fmt;

use sqlx::{FromRow, SqliteConnection, SqlitePool};
use thiserror::Error;
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime, UtcOffset};

use crate::{
    domain::{merge_queue::PullRequestNumber, repository::RepositoryId},
    security::CanonicalRepositoryName,
    telemetry::{
        commit_cost::{CommitCostTotal, CommitTrigger},
        run_cost::{bounded_breakdown, breakdown_key},
        trace::{self, CommitSha, DatabaseOperation},
        workflow::{WorkflowEvent, WorkflowRunAttempt, WorkflowRunId},
    },
};

use super::{sqlite_is_busy_or_locked, FinalizedRunCost};

const COMMIT_COST_PRUNE_BATCH_SIZE: i64 = 1_000;
const COMMIT_COST_TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// The SQL condition that the pending row `state` is due: quiet since `?1`, the settled cutoff,
/// with no commit-triggered run attempt of it still inside its grace period, or inactive since
/// `?2`, the maximum-age cutoff, whatever is pending.
///
/// The event list is the one [`CommitTrigger::of_event`] accepts. It is a macro rather than a
/// constant so `concat!` can splice it into `due` and `finalize` at compile time.
macro_rules! commit_cost_due {
    () => {
        "(state.last_activity_at <= ?2 OR (state.last_activity_at <= ?1 AND NOT EXISTS (\
             SELECT 1 FROM run_cost_finalizations AS pending \
             WHERE pending.repository_id = state.repository_id \
             AND pending.head_sha = state.commit_sha \
             AND pending.finalized_at IS NULL AND pending.abandoned_at IS NULL \
             AND pending.event IN ('pull_request', 'pull_request_target', 'push', 'merge_group')\
         )))"
    };
}

/// The times before which each finalization rule applies, fixed for one finalizer poll.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CommitCostCutoffs {
    /// A commit whose latest run finalized before this, with none pending, is settled.
    pub(crate) settled_before: OffsetDateTime,
    /// A commit with no activity since before this has reached its maximum age.
    pub(crate) inactive_before: OffsetDateTime,
}

/// One pending commit that a finalization rule now applies to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DueCommitCost {
    pub(crate) repository_id: RepositoryId,
    pub(crate) repository_name: CanonicalRepositoryName,
    pub(crate) commit_sha: CommitSha,
}

/// Durable per-commit cost state, finalized once into one `ci.commit.cost` record.
///
/// The record is built from the run totals stored against the commit and the vendor runs on it.
/// Run totals are keyed by run attempt and vendor runs by delivery, so a redelivered webhook
/// never counts twice.
#[derive(Clone)]
pub(crate) struct CommitCostStore {
    pool: SqlitePool,
}

impl CommitCostStore {
    /// Creates a commit cost store from an already migrated SQLite pool.
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Lists at most `limit` pending commits a finalization rule applies to, least recently
    /// active first.
    ///
    /// Like [`super::RunCostStore::due`], this runs on every poll, so it records no database span.
    ///
    /// # Errors
    ///
    /// Returns [`CommitCostStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`CommitCostStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn due(
        &self,
        cutoffs: CommitCostCutoffs,
        limit: i64,
    ) -> Result<Vec<DueCommitCost>, CommitCostStoreError> {
        let [settled_before, inactive_before] = format_cutoffs(cutoffs)?;
        let rows = sqlx::query_as::<_, StoredDueCommitCost>(concat!(
            "SELECT state.repository_id, repositories.full_name, state.commit_sha \
             FROM commit_costs AS state \
             JOIN repositories ON repositories.id = state.repository_id \
             WHERE state.finalized_at IS NULL AND ",
            commit_cost_due!(),
            " ORDER BY state.last_activity_at LIMIT ?3",
        ))
        .bind(settled_before)
        .bind(inactive_before)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(rows
            .into_iter()
            .filter_map(StoredDueCommitCost::into_due)
            .collect())
    }

    /// Builds one pending commit's record, marks it final and deletes the run totals it was built
    /// from.
    ///
    /// Like [`super::PullRequestCostStore::finalize`], the reads and writes share one transaction
    /// that takes SQLite's write lock first, and the rule is evaluated again under that lock with
    /// the same `cutoffs`, so a run total committed in between either is counted or leaves the
    /// commit for a later poll.
    ///
    /// # Returns
    ///
    /// The record, or `None` when the commit was already final or is no longer due.
    ///
    /// # Errors
    ///
    /// Returns [`CommitCostStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`CommitCostStoreError::Internal`] for any other persistence failure. On error nothing is
    /// stored and the commit stays pending.
    pub(crate) async fn finalize(
        &self,
        due: &DueCommitCost,
        cutoffs: CommitCostCutoffs,
    ) -> Result<Option<CommitCostTotal>, CommitCostStoreError> {
        let cutoffs = format_cutoffs(cutoffs)?;
        trace::instrument_database_operation(DatabaseOperation::CommitCostFinalize, async {
            let mut transaction = self
                .pool
                .begin_with("BEGIN IMMEDIATE")
                .await
                .map_err(map_sqlx_error)?;
            let Some(total) = commit_total_in(&mut transaction, due, &cutoffs)
                .await
                .map_err(map_sqlx_error)?
            else {
                return Ok(None);
            };
            let repository_id = due.repository_id.get();
            let commit_sha = due.commit_sha.as_str();
            sqlx::query(
                "UPDATE commit_costs SET \
                 finalized_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), partial = ?, \
                 total_usd = ? \
                 WHERE repository_id = ? AND commit_sha = ? AND finalized_at IS NULL",
            )
            .bind(total.partial)
            .bind(total.total_usd)
            .bind(repository_id)
            .bind(commit_sha)
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx_error)?;
            // the split has no foreign key to its total, so it is deleted before the totals that
            // select it
            for statement in [
                "DELETE FROM commit_run_cost_sources WHERE (repository_id, workflow_run_id, \
                 workflow_run_attempt) IN (SELECT repository_id, workflow_run_id, \
                 workflow_run_attempt FROM commit_run_costs \
                 WHERE repository_id = ?1 AND commit_sha = ?2)",
                "DELETE FROM commit_run_costs WHERE repository_id = ?1 AND commit_sha = ?2",
            ] {
                sqlx::query(statement)
                    .bind(repository_id)
                    .bind(commit_sha)
                    .execute(&mut *transaction)
                    .await
                    .map_err(map_sqlx_error)?;
            }
            transaction.commit().await.map_err(map_sqlx_error)?;
            Ok(Some(total))
        })
        .await
    }

    /// Deletes at most 1,000 finalized commits last active before `cutoff`, and at most 1,000
    /// pre-rollup markers written before it.
    ///
    /// Pending commits are never pruned: one inactive for the whole retention period is due by
    /// its maximum age first.
    ///
    /// # Errors
    ///
    /// Returns [`CommitCostStoreError::Unavailable`] when SQLite is busy or locked, and
    /// [`CommitCostStoreError::Internal`] for any other persistence failure.
    pub(crate) async fn prune_batch(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<u64, CommitCostStoreError> {
        trace::instrument_database_operation(DatabaseOperation::CommitCostPrune, async {
            let cutoff = format_timestamp(cutoff)?;
            let mut pruned = 0;
            for statement in [
                "DELETE FROM commit_costs WHERE rowid IN (\
                     SELECT rowid FROM commit_costs \
                     WHERE finalized_at IS NOT NULL AND last_activity_at < ?1 \
                     ORDER BY last_activity_at, rowid LIMIT ?2)",
                "DELETE FROM commit_costs_before_rollup WHERE rowid IN (\
                     SELECT rowid FROM commit_costs_before_rollup WHERE updated_at < ?1 \
                     ORDER BY updated_at, rowid LIMIT ?2)",
            ] {
                pruned += sqlx::query(statement)
                    .bind(&cutoff)
                    .bind(COMMIT_COST_PRUNE_BATCH_SIZE)
                    .execute(&self.pool)
                    .await
                    .map_err(map_sqlx_error)?
                    .rows_affected();
            }
            Ok(pruned)
        })
        .await
    }
}

/// Records cost activity for one commit on `connection`, inside the writer's transaction.
///
/// # Returns
///
/// Whether the commit's record was already final, so the caller can warn that the cost arrived
/// too late to be counted.
pub(crate) async fn record_commit_activity_in(
    connection: &mut SqliteConnection,
    repository_id: RepositoryId,
    commit_sha: &CommitSha,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar::<_, bool>(
        "INSERT INTO commit_costs (repository_id, commit_sha, last_activity_at) \
         VALUES (?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) \
         ON CONFLICT(repository_id, commit_sha) DO UPDATE SET \
         last_activity_at = excluded.last_activity_at \
         RETURNING finalized_at IS NOT NULL",
    )
    .bind(repository_id.get())
    .bind(commit_sha.as_str())
    .fetch_one(&mut *connection)
    .await
}

/// Stores one finalized run attempt's total, split by source, against its head commit when its
/// trigger counts toward the commit, inside the run finalization's transaction.
///
/// Any total the attempt already had is replaced, so finalizing it again never counts twice.
///
/// # Returns
///
/// Whether the commit's record was already final, in which case nothing is stored; `false` when
/// the attempt does not count toward a commit.
pub(crate) async fn record_commit_run_in(
    connection: &mut SqliteConnection,
    repository_id: RepositoryId,
    run_id: WorkflowRunId,
    run_attempt: WorkflowRunAttempt,
    finalized: &FinalizedRunCost,
) -> Result<bool, sqlx::Error> {
    for statement in [
        "DELETE FROM commit_run_cost_sources \
         WHERE repository_id = ? AND workflow_run_id = ? AND workflow_run_attempt = ?",
        "DELETE FROM commit_run_costs \
         WHERE repository_id = ? AND workflow_run_id = ? AND workflow_run_attempt = ?",
    ] {
        sqlx::query(statement)
            .bind(repository_id.get())
            .bind(run_id.get())
            .bind(run_attempt.get())
            .execute(&mut *connection)
            .await?;
    }
    let Some(commit_sha) = finalized
        .head_sha
        .as_ref()
        .filter(|_| CommitTrigger::of_event(finalized.event).is_some())
    else {
        return Ok(false);
    };
    if record_commit_activity_in(connection, repository_id, commit_sha).await? {
        return Ok(true);
    }
    sqlx::query(
        "INSERT INTO commit_run_costs \
         (repository_id, workflow_run_id, workflow_run_attempt, commit_sha, event, \
          pull_request_number, total_usd) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(repository_id.get())
    .bind(run_id.get())
    .bind(run_attempt.get())
    .bind(commit_sha.as_str())
    .bind(finalized.event.as_str())
    .bind(finalized.pull_request.map(PullRequestNumber::get))
    .bind(finalized.total_usd)
    .execute(&mut *connection)
    .await?;
    for (source, cost_usd) in &finalized.cost_by_source {
        sqlx::query(
            "INSERT INTO commit_run_cost_sources \
             (repository_id, workflow_run_id, workflow_run_attempt, source, cost_usd) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(repository_id.get())
        .bind(run_id.get())
        .bind(run_attempt.get())
        .bind(source)
        .bind(cost_usd)
        .execute(&mut *connection)
        .await?;
    }
    Ok(false)
}

/// Reads one pending commit's state and sums its stored costs, or `None` when it is already final
/// or no longer due at `cutoffs`.
async fn commit_total_in(
    connection: &mut SqliteConnection,
    due: &DueCommitCost,
    cutoffs: &[String; 2],
) -> Result<Option<CommitCostTotal>, sqlx::Error> {
    let repository_id = due.repository_id.get();
    let commit_sha = due.commit_sha.as_str();
    let [settled_before, inactive_before] = cutoffs;
    let partial = sqlx::query_scalar::<_, bool>(concat!(
        "SELECT EXISTS (SELECT 1 FROM commit_costs_before_rollup AS marker \
             WHERE marker.repository_id = state.repository_id \
             AND marker.commit_sha = state.commit_sha) \
         FROM commit_costs AS state \
         WHERE state.repository_id = ?3 AND state.commit_sha = ?4 \
         AND state.finalized_at IS NULL AND ",
        commit_cost_due!(),
    ))
    .bind(settled_before)
    .bind(inactive_before)
    .bind(repository_id)
    .bind(commit_sha)
    .fetch_optional(&mut *connection)
    .await?;
    let Some(partial) = partial else {
        return Ok(None);
    };
    let (attempts, runs, run_total_usd): (i64, i64, f64) = sqlx::query_as(
        "SELECT COUNT(*), COUNT(DISTINCT workflow_run_id), TOTAL(total_usd) \
         FROM commit_run_costs WHERE repository_id = ? AND commit_sha = ?",
    )
    .bind(repository_id)
    .bind(commit_sha)
    .fetch_one(&mut *connection)
    .await?;
    let events: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT event FROM commit_run_costs WHERE repository_id = ? AND commit_sha = ?",
    )
    .bind(repository_id)
    .bind(commit_sha)
    .fetch_all(&mut *connection)
    .await?;
    let run_sources: Vec<(String, f64)> = sqlx::query_as(
        "SELECT split.source, TOTAL(split.cost_usd) FROM commit_run_cost_sources AS split \
         JOIN commit_run_costs AS run ON run.repository_id = split.repository_id \
         AND run.workflow_run_id = split.workflow_run_id \
         AND run.workflow_run_attempt = split.workflow_run_attempt \
         WHERE run.repository_id = ? AND run.commit_sha = ? GROUP BY split.source",
    )
    .bind(repository_id)
    .bind(commit_sha)
    .fetch_all(&mut *connection)
    .await?;
    let vendors: Vec<(String, i64, f64)> = sqlx::query_as(
        "SELECT vendor, COUNT(*), TOTAL(cost_usd) FROM ci_vendor_runs \
         WHERE repository_id = ? AND head_sha = ? GROUP BY vendor",
    )
    .bind(repository_id)
    .bind(commit_sha)
    .fetch_all(&mut *connection)
    .await?;
    let pull_request: Option<i64> = sqlx::query_scalar(
        "SELECT pull_request_number FROM (\
             SELECT pull_request_number FROM commit_run_costs \
             WHERE repository_id = ?1 AND commit_sha = ?2 AND pull_request_number IS NOT NULL \
             UNION ALL \
             SELECT pull_request_number FROM ci_vendor_runs \
             WHERE repository_id = ?1 AND head_sha = ?2\
         ) GROUP BY pull_request_number ORDER BY COUNT(*) DESC, pull_request_number LIMIT 1",
    )
    .bind(repository_id)
    .bind(commit_sha)
    .fetch_optional(&mut *connection)
    .await?;
    let trigger = CommitTrigger::of_events(
        events
            .iter()
            .filter_map(|event| WorkflowEvent::from_stored(event)),
    );
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
    Ok(Some(CommitCostTotal {
        repository_name: due.repository_name.clone(),
        commit_sha: due.commit_sha.clone(),
        trigger,
        pull_request: pull_request.and_then(|number| PullRequestNumber::new(number).ok()),
        partial,
        runs,
        reruns: attempts - runs,
        vendor_runs,
        total_usd: run_total_usd + vendor_total_usd,
        cost_by_source,
        finalized_at: std::time::SystemTime::now(),
    }))
}

#[derive(FromRow)]
struct StoredDueCommitCost {
    repository_id: i64,
    full_name: String,
    commit_sha: String,
}

impl StoredDueCommitCost {
    fn into_due(self) -> Option<DueCommitCost> {
        Some(DueCommitCost {
            repository_id: RepositoryId::new(self.repository_id)?,
            repository_name: CanonicalRepositoryName::new(&self.full_name).ok()?,
            commit_sha: CommitSha::parse(&self.commit_sha).ok()?,
        })
    }
}

/// A stable, redacted commit cost persistence failure.
#[derive(Clone, Copy, Error, PartialEq, Eq)]
pub(crate) enum CommitCostStoreError {
    /// SQLite is temporarily busy or locked.
    #[error("commit cost storage is temporarily unavailable")]
    Unavailable,
    /// SQLite returned an unexpected persistence failure whose details were discarded.
    #[error("internal commit cost persistence failure")]
    Internal,
}

impl fmt::Debug for CommitCostStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "CommitCostStoreError::Unavailable",
            Self::Internal => "CommitCostStoreError::Internal",
        })
    }
}

/// Formats the two cutoffs in the `?1`, `?2` order [`commit_cost_due`] uses.
fn format_cutoffs(cutoffs: CommitCostCutoffs) -> Result<[String; 2], CommitCostStoreError> {
    Ok([
        format_timestamp(cutoffs.settled_before)?,
        format_timestamp(cutoffs.inactive_before)?,
    ])
}

fn format_timestamp(value: OffsetDateTime) -> Result<String, CommitCostStoreError> {
    value
        .to_offset(UtcOffset::UTC)
        .format(COMMIT_COST_TIMESTAMP_FORMAT)
        .map_err(|_| CommitCostStoreError::Internal)
}

fn map_sqlx_error(error: sqlx::Error) -> CommitCostStoreError {
    if sqlite_is_busy_or_locked(&error) {
        CommitCostStoreError::Unavailable
    } else {
        CommitCostStoreError::Internal
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use sqlx::SqlitePool;
    use time::{Duration, OffsetDateTime};

    use super::{CommitCostCutoffs, CommitCostStore, DueCommitCost};
    use crate::{
        domain::{merge_queue::PullRequestNumber, repository::RepositoryId},
        security::CanonicalRepositoryName,
        storage::{open_database, FinalizedRunCost, PendingRunCost, RunCostStore, VendorRunStore},
        telemetry::{
            commit_cost::{CommitCostTotal, CommitTrigger},
            run_cost::PullRequestResolution,
            trace::CommitSha,
            vendor::{VendorRun, VendorTrigger},
            workflow::{
                DisplayName, WorkflowConclusion, WorkflowEvent, WorkflowRunAttempt, WorkflowRunId,
            },
        },
    };

    const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const OTHER_SHA: &str = "1123456789abcdef0123456789abcdef01234567";

    struct Fixture {
        _directory: tempfile::TempDir,
        pool: SqlitePool,
        store: CommitCostStore,
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
            store: CommitCostStore::new(pool.clone()),
            runs: RunCostStore::new(pool.clone()),
            pool,
            repository_id: RepositoryId::new(id).expect("repository id is positive"),
        }
    }

    fn sha(value: &str) -> CommitSha {
        CommitSha::parse(value).expect("SHA parses")
    }

    fn pending(run_id: i64, run_attempt: i64, event: WorkflowEvent) -> PendingRunCost {
        PendingRunCost {
            run_id: WorkflowRunId::new(run_id).expect("run id is positive"),
            run_attempt: WorkflowRunAttempt::new(run_attempt).expect("attempt is positive"),
            event,
            workflow_name: None,
            workflow_file: None,
            conclusion: WorkflowConclusion::Success,
            head_sha: Some(sha(HEAD_SHA)),
            pull_request: None,
        }
    }

    fn finalized(
        pending: &PendingRunCost,
        pull_request: Option<i64>,
        sources: &[(&str, f64)],
    ) -> FinalizedRunCost {
        FinalizedRunCost {
            pull_request: pull_request
                .map(|number| PullRequestNumber::new(number).expect("PR is positive")),
            resolution: if pull_request.is_some() {
                PullRequestResolution::PullRequests
            } else {
                PullRequestResolution::None
            },
            total_usd: sources.iter().map(|(_, cost_usd)| cost_usd).sum(),
            price_table_version: None,
            event: pending.event,
            head_sha: pending.head_sha.clone(),
            cost_by_source: sources
                .iter()
                .map(|(source, cost_usd)| ((*source).to_owned(), *cost_usd))
                .collect(),
        }
    }

    impl Fixture {
        async fn schedule(&self, pending: &PendingRunCost) {
            self.runs
                .schedule(self.repository_id, pending, OffsetDateTime::now_utc())
                .await
                .expect("schedule succeeds");
        }

        /// Schedules and finalizes one attempt, returning whether its commit was already final.
        async fn finalize_run(&self, pending: &PendingRunCost, run: FinalizedRunCost) -> bool {
            self.schedule(pending).await;
            self.runs
                .finalize(
                    self.repository_id,
                    pending.run_id,
                    pending.run_attempt,
                    |_, _| (run, ()),
                )
                .await
                .expect("finalize succeeds")
                .expect("the attempt was pending")
                .commit_was_final
        }

        fn due_commit(&self) -> DueCommitCost {
            DueCommitCost {
                repository_id: self.repository_id,
                repository_name: CanonicalRepositoryName::new("owner/repo").expect("name is valid"),
                commit_sha: sha(HEAD_SHA),
            }
        }

        async fn due_shas(&self, cutoffs: CommitCostCutoffs) -> Vec<String> {
            self.store
                .due(cutoffs, 10)
                .await
                .expect("due query succeeds")
                .into_iter()
                .map(|due| due.commit_sha.as_str().to_owned())
                .collect()
        }

        async fn finalize(&self, cutoffs: CommitCostCutoffs) -> Option<CommitCostTotal> {
            self.store
                .finalize(&self.due_commit(), cutoffs)
                .await
                .expect("finalize succeeds")
        }

        async fn count(&self, table: &str) -> i64 {
            sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table}"))
                .fetch_one(&self.pool)
                .await
                .expect("count loads")
        }
    }

    fn nothing_elapsed() -> CommitCostCutoffs {
        let long_ago = OffsetDateTime::now_utc() - Duration::days(1);
        CommitCostCutoffs {
            settled_before: long_ago,
            inactive_before: long_ago,
        }
    }

    fn settled() -> CommitCostCutoffs {
        CommitCostCutoffs {
            settled_before: OffsetDateTime::now_utc() + Duration::hours(1),
            ..nothing_elapsed()
        }
    }

    fn inactive() -> CommitCostCutoffs {
        CommitCostCutoffs {
            inactive_before: OffsetDateTime::now_utc() + Duration::hours(1),
            ..nothing_elapsed()
        }
    }

    #[test]
    fn the_pending_run_check_names_exactly_the_commit_triggered_events() {
        let condition = commit_cost_due!();
        for event in WorkflowEvent::ALL {
            assert_eq!(
                condition.contains(&format!("'{}'", event.as_str())),
                CommitTrigger::of_event(event).is_some(),
                "{}",
                event.as_str()
            );
        }
    }

    #[tokio::test]
    async fn a_settled_commit_finalizes_once_with_its_runs_reruns_and_sources() {
        let fixture = fixture().await;
        let first = pending(1, 1, WorkflowEvent::PullRequest);
        let rerun = pending(1, 2, WorkflowEvent::PullRequest);
        let push = pending(2, 1, WorkflowEvent::Push);
        fixture
            .finalize_run(
                &first,
                finalized(&first, Some(42), &[("github_actions", 1.0)]),
            )
            .await;
        fixture
            .finalize_run(
                &rerun,
                finalized(
                    &rerun,
                    Some(42),
                    &[("github_actions", 1.0), ("inference", 0.5)],
                ),
            )
            .await;
        fixture
            .finalize_run(&push, finalized(&push, None, &[("github_actions", 2.0)]))
            .await;
        assert!(fixture.due_shas(nothing_elapsed()).await.is_empty());
        assert_eq!(fixture.finalize(nothing_elapsed()).await, None);

        assert_eq!(fixture.due_shas(settled()).await, [HEAD_SHA]);
        let total = fixture.finalize(settled()).await.expect("commit finalizes");

        assert_eq!(total.trigger, CommitTrigger::Push);
        assert_eq!(total.pull_request.map(PullRequestNumber::get), Some(42));
        assert_eq!((total.runs, total.reruns, total.vendor_runs), (2, 1, 0));
        assert!(!total.partial);
        assert_eq!(total.total_usd, 4.5);
        assert_eq!(
            total.cost_by_source,
            [
                ("github_actions".to_owned(), 4.0),
                ("inference".to_owned(), 0.5)
            ]
        );
        assert_eq!(fixture.finalize(settled()).await, None, "finalized once");
        assert!(fixture.due_shas(settled()).await.is_empty());
        assert_eq!(fixture.count("commit_run_costs").await, 0);
        assert_eq!(fixture.count("commit_run_cost_sources").await, 0);
    }

    #[tokio::test]
    async fn a_commit_waits_for_its_pending_runs_until_its_maximum_age() {
        let fixture = fixture().await;
        let done = pending(1, 1, WorkflowEvent::Push);
        fixture
            .finalize_run(&done, finalized(&done, None, &[("github_actions", 1.0)]))
            .await;
        fixture
            .schedule(&pending(2, 1, WorkflowEvent::MergeGroup))
            .await;

        assert!(fixture.due_shas(settled()).await.is_empty());
        assert_eq!(fixture.finalize(settled()).await, None);

        assert_eq!(fixture.due_shas(inactive()).await, [HEAD_SHA]);
        let total = fixture
            .finalize(inactive())
            .await
            .expect("commit finalizes");
        assert_eq!(
            total.trigger,
            CommitTrigger::Push,
            "only finalized runs count"
        );
        assert_eq!(total.total_usd, 1.0);
    }

    #[tokio::test]
    async fn a_pending_run_of_another_trigger_does_not_hold_the_commit() {
        let fixture = fixture().await;
        let done = pending(1, 1, WorkflowEvent::PullRequest);
        fixture
            .finalize_run(
                &done,
                finalized(&done, Some(42), &[("github_actions", 1.0)]),
            )
            .await;
        fixture
            .schedule(&pending(2, 1, WorkflowEvent::Schedule))
            .await;

        assert_eq!(fixture.due_shas(settled()).await, [HEAD_SHA]);
    }

    #[tokio::test]
    async fn runs_the_commit_did_not_trigger_are_left_out() {
        let fixture = fixture().await;
        for (run_id, event) in [
            (1, WorkflowEvent::Schedule),
            (2, WorkflowEvent::WorkflowDispatch),
            (3, WorkflowEvent::IssueComment),
            (4, WorkflowEvent::PullRequestReview),
            (5, WorkflowEvent::WorkflowRun),
        ] {
            let run = pending(run_id, 1, event);
            assert!(
                !fixture
                    .finalize_run(&run, finalized(&run, Some(42), &[("github_actions", 1.0)]))
                    .await
            );
        }

        assert_eq!(fixture.count("commit_costs").await, 0);
        assert_eq!(fixture.count("commit_run_costs").await, 0);
    }

    #[tokio::test]
    async fn a_refinalized_attempt_replaces_its_commit_total() {
        let fixture = fixture().await;
        let run = pending(1, 1, WorkflowEvent::PullRequest);
        fixture
            .finalize_run(&run, finalized(&run, Some(42), &[("github_actions", 1.0)]))
            .await;
        sqlx::query("DELETE FROM run_cost_finalizations")
            .execute(&fixture.pool)
            .await
            .expect("finalization clears");
        fixture
            .finalize_run(&run, finalized(&run, Some(42), &[("github_actions", 3.0)]))
            .await;

        let total = fixture.finalize(settled()).await.expect("commit finalizes");
        assert_eq!((total.runs, total.reruns), (1, 0));
        assert_eq!(total.total_usd, 3.0);
        assert_eq!(total.cost_by_source, [("github_actions".to_owned(), 3.0)]);
    }

    #[tokio::test]
    async fn a_run_after_finalization_is_late_and_not_stored() {
        let fixture = fixture().await;
        let run = pending(1, 1, WorkflowEvent::Push);
        assert!(
            !fixture
                .finalize_run(&run, finalized(&run, None, &[("github_actions", 1.0)]))
                .await
        );
        fixture.finalize(settled()).await.expect("commit finalizes");

        let late = pending(1, 2, WorkflowEvent::Push);
        assert!(
            fixture
                .finalize_run(&late, finalized(&late, None, &[("github_actions", 1.0)]))
                .await
        );
        assert_eq!(fixture.count("commit_run_costs").await, 0);
        assert!(fixture.due_shas(settled()).await.is_empty());
    }

    #[tokio::test]
    async fn vendor_runs_on_the_commit_count_and_decide_a_tied_pull_request() {
        let fixture = fixture().await;
        let run = pending(1, 1, WorkflowEvent::PullRequest);
        fixture
            .finalize_run(&run, finalized(&run, Some(43), &[("github_actions", 1.0)]))
            .await;
        let vendors = VendorRunStore::new(fixture.pool.clone());
        for (pull_request, head_sha) in [(41, HEAD_SHA), (99, OTHER_SHA)] {
            vendors
                .record(
                    fixture.repository_id,
                    &VendorRun {
                        repository_name: CanonicalRepositoryName::new("owner/repo")
                            .expect("name is valid"),
                        vendor: DisplayName::sanitize("review_bot").expect("name is valid"),
                        trigger: VendorTrigger::PullRequestReview,
                        cost_usd: 0.25,
                        price_table_version: "0123456789ab".to_owned(),
                        pull_request: Some(
                            PullRequestNumber::new(pull_request).expect("PR is positive"),
                        ),
                        head_sha: Some(sha(head_sha)),
                        received_at: SystemTime::now(),
                    },
                )
                .await
                .expect("vendor run persists");
        }

        let total = fixture.finalize(settled()).await.expect("commit finalizes");
        assert_eq!(total.vendor_runs, 1);
        assert_eq!(total.total_usd, 1.25);
        assert_eq!(
            total.pull_request.map(PullRequestNumber::get),
            Some(41),
            "a one-to-one tie goes to the lower number"
        );
        assert_eq!(
            total.cost_by_source,
            [
                ("github_actions".to_owned(), 1.0),
                ("review_bot".to_owned(), 0.25)
            ]
        );
    }

    #[tokio::test]
    async fn a_commit_with_runs_from_before_the_rollup_is_partial() {
        let fixture = fixture().await;
        sqlx::query(
            "INSERT INTO commit_costs_before_rollup (repository_id, commit_sha, updated_at) \
             VALUES (?, ?, '2026-11-18T10:00:00.000Z')",
        )
        .bind(fixture.repository_id.get())
        .bind(HEAD_SHA)
        .execute(&fixture.pool)
        .await
        .expect("marker inserts");
        let run = pending(1, 1, WorkflowEvent::MergeGroup);
        fixture
            .finalize_run(&run, finalized(&run, None, &[("github_actions", 1.0)]))
            .await;

        let total = fixture.finalize(settled()).await.expect("commit finalizes");
        assert!(total.partial);
        assert_eq!(total.trigger, CommitTrigger::MergeGroup);
        assert_eq!(total.pull_request, None);
    }

    #[tokio::test]
    async fn prune_removes_only_old_finalized_commits_and_markers() {
        let fixture = fixture().await;
        let run = pending(1, 1, WorkflowEvent::Push);
        fixture
            .finalize_run(&run, finalized(&run, None, &[("github_actions", 1.0)]))
            .await;
        sqlx::query(
            "INSERT INTO commit_costs_before_rollup (repository_id, commit_sha, updated_at) \
             VALUES (?, ?, '2026-01-01T00:00:00.000Z')",
        )
        .bind(fixture.repository_id.get())
        .bind(OTHER_SHA)
        .execute(&fixture.pool)
        .await
        .expect("marker inserts");
        sqlx::query("UPDATE commit_costs SET last_activity_at = '2026-01-01T00:00:00.000Z'")
            .execute(&fixture.pool)
            .await
            .expect("commit ages");
        let cutoff = OffsetDateTime::now_utc() - Duration::days(1);

        assert_eq!(
            fixture
                .store
                .prune_batch(cutoff)
                .await
                .expect("prune succeeds"),
            1,
            "only the marker; a pending commit is kept"
        );
        fixture
            .finalize(inactive())
            .await
            .expect("commit finalizes");
        sqlx::query("UPDATE commit_costs SET last_activity_at = '2026-01-01T00:00:00.000Z'")
            .execute(&fixture.pool)
            .await
            .expect("commit ages");

        assert_eq!(
            fixture
                .store
                .prune_batch(cutoff)
                .await
                .expect("prune succeeds"),
            1
        );
        assert_eq!(fixture.count("commit_costs").await, 0);
    }
}
