use std::time::Duration;

use sqlx::SqlitePool;
use time::OffsetDateTime;
use tokio::sync::watch;
use tracing::{warn, Instrument};

use crate::{
    error::ErrorCorrelationId,
    lifecycle,
    pr_costs::{cost_duration, DEFAULT_PR_COST_RETENTION_DAYS, PULL_REQUEST_COST_POLL_INTERVAL},
    run_costs::FinalizerClock,
    storage::{CommitCostCutoffs, CommitCostStore, DueCommitCost},
    telemetry::{
        commit_cost::log_commit_cost_finalized,
        trace::{self, Operation, OperationOutcome},
        WorkflowTraceEmitter,
    },
};

/// The most commits finalized in one poll.
const COMMIT_COST_BATCH_SIZE: i64 = 100;
/// The default quiet period after a commit's latest run before its cost record is finalized.
pub const DEFAULT_COMMIT_COST_SETTLE_SECONDS: u64 = 21_600;

/// When a pending commit's cost record is finalized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CommitCostPolicy {
    settle: time::Duration,
    max_age: time::Duration,
}

/// A commit cost duration was zero or outside the supported range.
#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
#[error("commit cost duration is invalid")]
pub struct CommitCostPolicyError;

impl CommitCostPolicy {
    /// Validates positive durations representable by `time`.
    ///
    /// # Parameters
    ///
    /// * `settle` - The quiet period after a commit's latest run, once none of its runs is
    ///   pending.
    /// * `max_age` - How long a commit may go without activity before it is finalized whatever
    ///   is pending; also its state retention.
    ///
    /// # Errors
    ///
    /// Returns [`CommitCostPolicyError`] when either duration is zero or too large.
    pub fn new(settle: Duration, max_age: Duration) -> Result<Self, CommitCostPolicyError> {
        Ok(Self {
            settle: cost_duration(settle).ok_or(CommitCostPolicyError)?,
            max_age: cost_duration(max_age).ok_or(CommitCostPolicyError)?,
        })
    }

    /// Returns the cutoffs that apply at `now`, or `None` when one falls outside `time`'s range.
    fn cutoffs(&self, now: OffsetDateTime) -> Option<CommitCostCutoffs> {
        Some(CommitCostCutoffs {
            settled_before: now.checked_sub(self.settle)?,
            inactive_before: now.checked_sub(self.max_age)?,
        })
    }
}

impl Default for CommitCostPolicy {
    fn default() -> Self {
        Self {
            settle: time::Duration::seconds(DEFAULT_COMMIT_COST_SETTLE_SECONDS as i64),
            max_age: time::Duration::days(DEFAULT_PR_COST_RETENTION_DAYS as i64),
        }
    }
}

/// Finalizes each commit's cost record once its runs have been quiet for the settle period.
///
/// Pending commits live in SQLite, so one that comes due while the process is down is finalized
/// by the next process. A failed finalization leaves the commit pending for the next poll.
#[derive(Clone)]
pub(crate) struct CommitCostFinalizer {
    store: CommitCostStore,
    emitter: WorkflowTraceEmitter,
    policy: CommitCostPolicy,
}

impl CommitCostFinalizer {
    /// Creates a finalizer over an already migrated SQLite pool.
    pub(crate) fn new(
        pool: &SqlitePool,
        emitter: WorkflowTraceEmitter,
        policy: CommitCostPolicy,
    ) -> Self {
        Self {
            store: CommitCostStore::new(pool.clone()),
            emitter,
            policy,
        }
    }

    /// Polls every [`PULL_REQUEST_COST_POLL_INTERVAL`] until cancellation, starting at once.
    pub(crate) async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let clock = FinalizerClock::start();
        let mut ticker = tokio::time::interval(PULL_REQUEST_COST_POLL_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                () = lifecycle::wait_for_cancellation(&mut shutdown) => return,
                _ = ticker.tick() => {
                    self.finalize_due(clock.now(), &shutdown).await;
                }
            }
        }
    }

    /// Finalizes at most one batch of commits due at `now`.
    ///
    /// Shutdown is honored between commits, never during one.
    ///
    /// # Returns
    ///
    /// The number of commits this call finalized.
    pub(crate) async fn finalize_due(
        &self,
        now: OffsetDateTime,
        shutdown: &watch::Receiver<bool>,
    ) -> usize {
        let Some(cutoffs) = self.policy.cutoffs(now) else {
            warn_finalization_failed("cutoff_out_of_range");
            return 0;
        };
        let due = match self.store.due(cutoffs, COMMIT_COST_BATCH_SIZE).await {
            Ok(due) => due,
            Err(_) => {
                warn_finalization_failed("due_lookup_failed");
                return 0;
            }
        };
        let mut finalized = 0;
        for commit in due {
            if *shutdown.borrow() {
                break;
            }
            let span = trace::operation_span(Operation::CommitCostFinalize);
            let outcome = self
                .finalize(&commit, cutoffs)
                .instrument(span.clone())
                .await;
            trace::set_status(
                &span,
                match outcome {
                    Some(true) => OperationOutcome::Success,
                    Some(false) => OperationOutcome::NoOp,
                    None => OperationOutcome::Failure,
                },
            );
            match outcome {
                Some(true) => finalized += 1,
                Some(false) => {}
                None => warn_finalization_failed("finalize_failed"),
            }
        }
        finalized
    }

    /// Returns whether this call finalized `due`, or `None` when the store failed.
    async fn finalize(&self, due: &DueCommitCost, cutoffs: CommitCostCutoffs) -> Option<bool> {
        let Some(total) = self.store.finalize(due, cutoffs).await.ok()? else {
            return Some(false);
        };
        self.emitter.emit_commit_cost(&total);
        log_commit_cost_finalized(&total);
        Some(true)
    }
}

fn warn_finalization_failed(outcome: &'static str) {
    let error_correlation_id = ErrorCorrelationId::new();
    warn!(
        parent: None,
        outcome,
        %error_correlation_id,
        "ci commit cost finalization failed"
    );
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use time::OffsetDateTime;
    use tokio::sync::watch;

    use super::{CommitCostFinalizer, CommitCostPolicy, CommitCostPolicyError};
    use crate::{
        domain::repository::RepositoryId,
        storage::{open_database, FinalizedRunCost, PendingRunCost, RunCostStore},
        telemetry::{
            run_cost::PullRequestResolution,
            trace::CommitSha,
            workflow::{
                WorkflowConclusion, WorkflowEvent, WorkflowRunAttempt, WorkflowRunId,
                WorkflowTraceEmitter,
            },
        },
    };

    const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    #[test]
    fn the_policy_rejects_zero_and_unrepresentable_durations() {
        let valid = Duration::from_secs(1);
        assert!(CommitCostPolicy::new(valid, valid).is_ok());
        for invalid in [Duration::ZERO, Duration::MAX] {
            assert_eq!(
                CommitCostPolicy::new(invalid, valid),
                Err(CommitCostPolicyError)
            );
            assert_eq!(
                CommitCostPolicy::new(valid, invalid),
                Err(CommitCostPolicyError)
            );
        }
    }

    #[tokio::test]
    async fn a_settled_commit_is_finalized_once_by_the_background_finalizer() {
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
        let runs = RunCostStore::new(pool.clone());
        let head_sha = CommitSha::parse(HEAD_SHA).expect("SHA parses");
        let pending = PendingRunCost {
            run_id: WorkflowRunId::new(7).expect("run id is positive"),
            run_attempt: WorkflowRunAttempt::new(1).expect("attempt is positive"),
            event: WorkflowEvent::Push,
            workflow_name: None,
            workflow_file: None,
            conclusion: WorkflowConclusion::Success,
            head_sha: Some(head_sha.clone()),
            pull_request: None,
        };
        runs.schedule(repository_id, &pending, OffsetDateTime::now_utc())
            .await
            .expect("schedule succeeds");
        runs.finalize_pending(
            repository_id,
            &pending,
            FinalizedRunCost {
                pull_request: None,
                resolution: PullRequestResolution::None,
                total_usd: 1.0,
                price_table_version: None,
                event: WorkflowEvent::Push,
                head_sha: Some(head_sha),
                cost_by_source: vec![("github_actions".to_owned(), 1.0)],
            },
        )
        .await
        .expect("finalize succeeds");
        let finalizer = CommitCostFinalizer::new(
            &pool,
            WorkflowTraceEmitter::disabled(),
            CommitCostPolicy::new(Duration::from_secs(60), Duration::from_secs(86_400))
                .expect("policy is valid"),
        );
        let (_sender, shutdown) = watch::channel(false);

        let now = OffsetDateTime::now_utc();
        assert_eq!(
            finalizer.finalize_due(now, &shutdown).await,
            0,
            "not settled"
        );
        let later = now + time::Duration::minutes(2);
        assert_eq!(finalizer.finalize_due(later, &shutdown).await, 1);
        assert_eq!(finalizer.finalize_due(later, &shutdown).await, 0, "once");
    }
}
