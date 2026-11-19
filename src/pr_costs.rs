use std::time::Duration;

use sqlx::SqlitePool;
use time::OffsetDateTime;
use tokio::sync::watch;
use tracing::{warn, Instrument};

use crate::{
    error::ErrorCorrelationId,
    lifecycle,
    run_costs::FinalizerClock,
    storage::{DuePullRequestCost, PullRequestCostCutoffs, PullRequestCostStore},
    telemetry::{
        pr_cost::log_pull_request_cost_finalized,
        trace::{self, Operation, OperationOutcome},
        WorkflowTraceEmitter,
    },
};

/// How often the finalizer looks for pull requests a finalization rule applies to.
pub(crate) const PULL_REQUEST_COST_POLL_INTERVAL: Duration = Duration::from_secs(60);
/// The most pull requests finalized in one poll.
const PULL_REQUEST_COST_BATCH_SIZE: i64 = 100;
/// The default quiet period after a merged pull request's latest post-merge run.
pub const DEFAULT_PR_COST_SETTLE_SECONDS: u64 = 1_800;
/// The default wait for a merged pull request's first post-merge run.
pub const DEFAULT_PR_COST_POST_MERGE_TIMEOUT_SECONDS: u64 = 21_600;
/// The default wait before a pull request closed without merging is finalized.
pub const DEFAULT_PR_COST_CLOSED_GRACE_SECONDS: u64 = 3_600;
/// The default days without activity before a pull request is finalized and its state pruned.
pub const DEFAULT_PR_COST_RETENTION_DAYS: u64 = 90;

/// When a pending pull request's cost record is finalized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PullRequestCostPolicy {
    settle: time::Duration,
    post_merge_timeout: time::Duration,
    closed_grace: time::Duration,
    max_age: time::Duration,
}

/// A pull-request cost duration was zero or outside the supported range.
#[derive(Clone, Copy, Debug, thiserror::Error, PartialEq, Eq)]
#[error("pull-request cost duration is invalid")]
pub struct PullRequestCostPolicyError;

impl PullRequestCostPolicy {
    /// Validates positive durations representable by `time`.
    ///
    /// # Parameters
    ///
    /// * `settle` - The quiet period after a merged pull request's latest post-merge run.
    /// * `post_merge_timeout` - How long a merged pull request waits for its first post-merge run.
    /// * `closed_grace` - How long a pull request closed without merging waits.
    /// * `max_age` - How long a pull request may go without activity; also its state retention.
    ///
    /// # Errors
    ///
    /// Returns [`PullRequestCostPolicyError`] when any duration is zero or too large.
    pub fn new(
        settle: Duration,
        post_merge_timeout: Duration,
        closed_grace: Duration,
        max_age: Duration,
    ) -> Result<Self, PullRequestCostPolicyError> {
        let validate = |duration: Duration| {
            if duration.is_zero() {
                return Err(PullRequestCostPolicyError);
            }
            time::Duration::try_from(duration).map_err(|_| PullRequestCostPolicyError)
        };
        Ok(Self {
            settle: validate(settle)?,
            post_merge_timeout: validate(post_merge_timeout)?,
            closed_grace: validate(closed_grace)?,
            max_age: validate(max_age)?,
        })
    }

    /// Returns the cutoffs that apply at `now`, or `None` when one falls outside `time`'s range.
    fn cutoffs(&self, now: OffsetDateTime) -> Option<PullRequestCostCutoffs> {
        Some(PullRequestCostCutoffs {
            settled_before: now.checked_sub(self.settle)?,
            merged_before: now.checked_sub(self.post_merge_timeout)?,
            closed_before: now.checked_sub(self.closed_grace)?,
            inactive_before: now.checked_sub(self.max_age)?,
        })
    }
}

impl Default for PullRequestCostPolicy {
    fn default() -> Self {
        Self {
            settle: time::Duration::seconds(DEFAULT_PR_COST_SETTLE_SECONDS as i64),
            post_merge_timeout: time::Duration::seconds(
                DEFAULT_PR_COST_POST_MERGE_TIMEOUT_SECONDS as i64,
            ),
            closed_grace: time::Duration::seconds(DEFAULT_PR_COST_CLOSED_GRACE_SECONDS as i64),
            max_age: time::Duration::days(DEFAULT_PR_COST_RETENTION_DAYS as i64),
        }
    }
}

/// Finalizes each pull request's cost record once a finalization rule applies to it.
///
/// Pending pull requests live in SQLite, so one that comes due while the process is down is
/// finalized by the next process. A failed finalization leaves the pull request pending for the
/// next poll.
#[derive(Clone)]
pub(crate) struct PullRequestCostFinalizer {
    store: PullRequestCostStore,
    emitter: WorkflowTraceEmitter,
    policy: PullRequestCostPolicy,
}

impl PullRequestCostFinalizer {
    /// Creates a finalizer over an already migrated SQLite pool.
    pub(crate) fn new(
        pool: &SqlitePool,
        emitter: WorkflowTraceEmitter,
        policy: PullRequestCostPolicy,
    ) -> Self {
        Self {
            store: PullRequestCostStore::new(pool.clone()),
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

    /// Finalizes at most one batch of pull requests due at `now`.
    ///
    /// Shutdown is honored between pull requests, never during one.
    ///
    /// # Returns
    ///
    /// The number of pull requests this call finalized.
    pub(crate) async fn finalize_due(
        &self,
        now: OffsetDateTime,
        shutdown: &watch::Receiver<bool>,
    ) -> usize {
        let Some(cutoffs) = self.policy.cutoffs(now) else {
            warn_finalization_failed("cutoff_out_of_range");
            return 0;
        };
        let due = match self.store.due(cutoffs, PULL_REQUEST_COST_BATCH_SIZE).await {
            Ok(due) => due,
            Err(_) => {
                warn_finalization_failed("due_lookup_failed");
                return 0;
            }
        };
        let mut finalized = 0;
        for pull_request in due {
            if *shutdown.borrow() {
                break;
            }
            let span = trace::operation_span(Operation::PullRequestCostFinalize);
            let outcome = self.finalize(&pull_request).instrument(span.clone()).await;
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
    async fn finalize(&self, due: &DuePullRequestCost) -> Option<bool> {
        let total = self.store.finalize(due).await.ok()?;
        let Some(total) = total else {
            return Some(false);
        };
        self.emitter.emit_pull_request_cost(&total);
        log_pull_request_cost_finalized(&total);
        Some(true)
    }
}

fn warn_finalization_failed(outcome: &'static str) {
    let error_correlation_id = ErrorCorrelationId::new();
    warn!(
        parent: None,
        outcome,
        %error_correlation_id,
        "ci pr cost finalization failed"
    );
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use time::OffsetDateTime;
    use tokio::sync::watch;

    use super::{PullRequestCostFinalizer, PullRequestCostPolicy, PullRequestCostPolicyError};
    use crate::{
        domain::{merge_queue::PullRequestNumber, repository::RepositoryId},
        storage::{
            open_database, PullRequestActivity, PullRequestCostStore, PullRequestCostTransition,
        },
        telemetry::WorkflowTraceEmitter,
    };

    #[test]
    fn the_policy_rejects_zero_and_unrepresentable_durations() {
        let valid = Duration::from_secs(1);
        assert!(PullRequestCostPolicy::new(valid, valid, valid, valid).is_ok());
        for invalid in [Duration::ZERO, Duration::MAX] {
            for position in 0..4 {
                let mut durations = [valid; 4];
                durations[position] = invalid;
                assert_eq!(
                    PullRequestCostPolicy::new(
                        durations[0],
                        durations[1],
                        durations[2],
                        durations[3]
                    ),
                    Err(PullRequestCostPolicyError)
                );
            }
        }
    }

    #[tokio::test]
    async fn a_due_pull_request_is_finalized_once_and_shutdown_stops_the_batch() {
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
        let store = PullRequestCostStore::new(pool.clone());
        for number in [41, 42] {
            store
                .record_activity(
                    repository_id,
                    &PullRequestActivity {
                        number: PullRequestNumber::new(number).expect("PR is positive"),
                        created_at: None,
                        transition: PullRequestCostTransition::Closed {
                            merged: false,
                            merge_commit_sha: None,
                            closed_at: OffsetDateTime::now_utc(),
                        },
                    },
                )
                .await
                .expect("activity records");
        }
        let finalizer = PullRequestCostFinalizer::new(
            &pool,
            WorkflowTraceEmitter::disabled(),
            PullRequestCostPolicy::default(),
        );
        let after_grace = OffsetDateTime::now_utc() + time::Duration::hours(2);

        let (_shutdown_sender, running) = watch::channel(false);
        assert_eq!(
            finalizer
                .finalize_due(OffsetDateTime::now_utc(), &running)
                .await,
            0,
            "nothing is due inside the closed grace period"
        );
        let (_stopped_sender, stopped) = watch::channel(true);
        assert_eq!(finalizer.finalize_due(after_grace, &stopped).await, 0);
        assert_eq!(finalizer.finalize_due(after_grace, &running).await, 2);
        assert_eq!(finalizer.finalize_due(after_grace, &running).await, 0);
        let finalized: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pull_request_costs \
             WHERE finalized_by = 'closed_grace' AND partial = 1 AND total_usd = 0",
        )
        .fetch_one(&pool)
        .await
        .expect("state loads");
        assert_eq!(
            finalized, 2,
            "no creation time is known, so both are partial"
        );
    }
}
