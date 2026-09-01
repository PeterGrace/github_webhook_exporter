use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use time::OffsetDateTime;
use tokio::{sync::watch, time::Instant};
use tracing::{info, warn, Instrument};

use crate::{
    config::DEFAULT_REQUIRED_CHECK_TTL_SECONDS,
    error::ErrorCorrelationId,
    lifecycle,
    storage::{
        DeliveryStore, DeliveryStoreError, MergeQueueStore, MergeQueueStoreError,
        RequiredCheckStore, RequiredCheckStoreError, WorkflowJobLinkStore,
        WorkflowJobLinkStoreError, WorkflowRunStore, WorkflowRunStoreError,
    },
    telemetry::trace::{self, Operation, OperationOutcome},
};

const FULL_PRUNE_BATCH_SIZE: u64 = 1_000;

/// Validated scheduling and age limits for delivery, workflow-run, and merge-queue retention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionConfig {
    interval: Duration,
    delivery_retention: time::Duration,
    merge_queue_retention: time::Duration,
}

impl RetentionConfig {
    /// Validates a positive prune interval and retention durations representable by `time`.
    ///
    /// # Errors
    ///
    /// Returns [`RetentionError::InvalidInterval`] for a zero interval and
    /// [`RetentionError::InvalidRetention`] when either retention duration is zero or cannot be
    /// represented safely for cutoff calculation.
    pub fn new(
        interval: Duration,
        delivery_retention: Duration,
        merge_queue_retention: Duration,
    ) -> Result<Self, RetentionError> {
        if interval.is_zero() {
            return Err(RetentionError::InvalidInterval);
        }
        let delivery_retention = validate_retention(delivery_retention)?;
        let merge_queue_retention = validate_retention(merge_queue_retention)?;
        Ok(Self {
            interval,
            delivery_retention,
            merge_queue_retention,
        })
    }
}

/// A stable retention configuration failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum RetentionError {
    /// The scheduling interval was zero.
    #[error("retention prune interval must be positive")]
    InvalidInterval,
    /// A retention duration was zero or outside the supported range.
    #[error("retention duration is invalid")]
    InvalidRetention,
}

fn validate_retention(retention: Duration) -> Result<time::Duration, RetentionError> {
    if retention.is_zero() {
        return Err(RetentionError::InvalidRetention);
    }
    time::Duration::try_from(retention).map_err(|_| RetentionError::InvalidRetention)
}

/// Runs scheduled bounded delivery, workflow-run, and merge-queue pruning until cancellation.
///
/// The first pass starts only after one full interval, and missed ticks are skipped. A shutdown
/// received during an active SQLite batch allows that batch to finish, then prevents another batch
/// from starting. All workloads use cutoffs fixed at the beginning of each scheduled pass;
/// workflow-run context shares the processed-delivery cutoff.
pub async fn run_retention(
    delivery_store: DeliveryStore,
    merge_queue_store: MergeQueueStore,
    config: RetentionConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let stores = DerivedRetentionStores::new(delivery_store.pool());
    let start = Instant::now() + config.interval;
    let mut ticker = tokio::time::interval_at(start, config.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            () = lifecycle::wait_for_cancellation(&mut shutdown) => return,
            _ = ticker.tick() => {
                run_traced_retention_pass(&delivery_store, &merge_queue_store, &stores, config, &shutdown)
                    .await;
            }
        }
    }
}

async fn run_traced_retention_pass(
    delivery_store: &DeliveryStore,
    merge_queue_store: &MergeQueueStore,
    derived_stores: &DerivedRetentionStores,
    config: RetentionConfig,
    shutdown: &watch::Receiver<bool>,
) {
    let retention_span = trace::operation_span(Operation::RetentionRun);
    let outcome = prune_retention_pass(
        delivery_store,
        merge_queue_store,
        derived_stores,
        config,
        shutdown,
    )
    .instrument(retention_span.clone())
    .await;
    trace::set_status(&retention_span, outcome.operation_outcome());
}

/// The retention-owned stores derived from the shared pool rather than from application state.
///
/// Retention only ever prunes, so it constructs these itself instead of borrowing the request
/// handlers' instances.
struct DerivedRetentionStores {
    workflow_run: WorkflowRunStore,
    workflow_job_link: WorkflowJobLinkStore,
    required_check: RequiredCheckStore,
}

impl DerivedRetentionStores {
    fn new(pool: &sqlx::SqlitePool) -> Self {
        Self {
            workflow_run: WorkflowRunStore::new(pool.clone()),
            workflow_job_link: WorkflowJobLinkStore::new(pool.clone()),
            // Pruning works from an explicit cutoff, so the cache time-to-live configured here is
            // never consulted; only reads honor it.
            required_check: RequiredCheckStore::new(
                pool.clone(),
                Duration::from_secs(DEFAULT_REQUIRED_CHECK_TTL_SECONDS),
            ),
        }
    }
}

/// Runs exactly one traced retention pass for deterministic test coverage.
#[cfg(test)]
pub(crate) async fn run_retention_once(
    delivery_store: &DeliveryStore,
    merge_queue_store: &MergeQueueStore,
    config: RetentionConfig,
    shutdown: &watch::Receiver<bool>,
) {
    let derived_stores = DerivedRetentionStores::new(delivery_store.pool());
    run_traced_retention_pass(
        delivery_store,
        merge_queue_store,
        &derived_stores,
        config,
        shutdown,
    )
    .await;
}

async fn prune_retention_pass(
    delivery_store: &DeliveryStore,
    merge_queue_store: &MergeQueueStore,
    derived_stores: &DerivedRetentionStores,
    config: RetentionConfig,
    shutdown: &watch::Receiver<bool>,
) -> RetentionPassOutcome {
    let pass_started_at = OffsetDateTime::now_utc();
    let delivery_cutoff = pass_started_at.checked_sub(config.delivery_retention);
    let merge_queue_cutoff = pass_started_at.checked_sub(config.merge_queue_retention);
    // Workflow correlation and required-check rows are short-lived caches keyed to deliveries, so
    // they share the delivery cutoff rather than carrying retention settings of their own.
    prune_retention_workloads(
        &[
            (delivery_store, delivery_cutoff),
            (merge_queue_store, merge_queue_cutoff),
            (&derived_stores.workflow_run, delivery_cutoff),
            (&derived_stores.workflow_job_link, delivery_cutoff),
            (&derived_stores.required_check, delivery_cutoff),
        ],
        shutdown,
    )
    .await
}

/// Prunes each workload in order, stopping early when shutdown is observed between stores.
///
/// # Parameters
///
/// * `workloads` - The stores to prune, each paired with its cutoff, in the order they run.
/// * `shutdown` - The process-wide cancellation channel, checked between stores.
///
/// # Returns
///
/// The combined pass outcome. A pass that stops early because of shutdown reports `Cancelled` even
/// when every store it did reach succeeded.
async fn prune_retention_workloads(
    workloads: &[(&dyn PrunableStore, Option<OffsetDateTime>)],
    shutdown: &watch::Receiver<bool>,
) -> RetentionPassOutcome {
    let mut outcome = RetentionPassOutcome::Success;
    let mut remaining = workloads.iter();
    while let Some((store, cutoff)) = remaining.next() {
        let store_outcome = prune_store(*store, *cutoff, shutdown).await;
        outcome = outcome.combine(store_outcome);
        // Shutdown is honored between stores, never mid-batch: an active SQLite batch finishes,
        // and only the stores that never started are reported as cancelled.
        if (*shutdown.borrow() || store_outcome == StorePruneOutcome::Cancelled)
            && remaining.len() > 0
        {
            return outcome.combine(StorePruneOutcome::Cancelled);
        }
    }
    outcome
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RetentionPassOutcome {
    Success,
    Cancelled,
    Failure,
}

impl RetentionPassOutcome {
    fn combine(self, outcome: StorePruneOutcome) -> Self {
        match (self, outcome) {
            (Self::Failure, StorePruneOutcome::Completed | StorePruneOutcome::Cancelled)
            | (Self::Failure, StorePruneOutcome::Failed)
            | (Self::Success | Self::Cancelled, StorePruneOutcome::Failed) => Self::Failure,
            (Self::Cancelled, StorePruneOutcome::Completed | StorePruneOutcome::Cancelled)
            | (Self::Success, StorePruneOutcome::Cancelled) => Self::Cancelled,
            (Self::Success, StorePruneOutcome::Completed) => Self::Success,
        }
    }

    fn operation_outcome(self) -> OperationOutcome {
        match self {
            Self::Success => OperationOutcome::Success,
            Self::Cancelled => OperationOutcome::Cancelled,
            Self::Failure => OperationOutcome::Failure,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StorePruneOutcome {
    Completed,
    Cancelled,
    Failed,
}

/// One durable store that retention prunes in bounded batches.
///
/// The trait is object-safe (via [`async_trait`]) so a pass can iterate a heterogeneous list of
/// stores instead of repeating the same sequence-and-check block per store. Store errors are
/// already discarded at the call site, so implementations collapse them into
/// [`StorePruneFailure`] rather than surfacing an associated error type.
#[async_trait]
trait PrunableStore: Sync {
    /// Returns the fixed workload name recorded in retention logs.
    fn workload(&self) -> &'static str;

    /// Deletes at most one bounded batch of rows last updated before `cutoff`.
    async fn prune_batch_erased(&self, cutoff: OffsetDateTime) -> Result<u64, StorePruneFailure>;
}

/// A redacted store pruning failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct StorePruneFailure;

#[async_trait]
impl PrunableStore for DeliveryStore {
    fn workload(&self) -> &'static str {
        "delivery"
    }

    async fn prune_batch_erased(&self, cutoff: OffsetDateTime) -> Result<u64, StorePruneFailure> {
        DeliveryStore::prune_batch(self, cutoff)
            .await
            .map_err(|_: DeliveryStoreError| StorePruneFailure)
    }
}

#[async_trait]
impl PrunableStore for MergeQueueStore {
    fn workload(&self) -> &'static str {
        "merge_queue"
    }

    async fn prune_batch_erased(&self, cutoff: OffsetDateTime) -> Result<u64, StorePruneFailure> {
        self.prune_completed_batch(cutoff)
            .await
            .map_err(|_: MergeQueueStoreError| StorePruneFailure)
    }
}

#[async_trait]
impl PrunableStore for WorkflowRunStore {
    fn workload(&self) -> &'static str {
        "workflow_run"
    }

    async fn prune_batch_erased(&self, cutoff: OffsetDateTime) -> Result<u64, StorePruneFailure> {
        WorkflowRunStore::prune_batch(self, cutoff)
            .await
            .map_err(|_: WorkflowRunStoreError| StorePruneFailure)
    }
}

#[async_trait]
impl PrunableStore for RequiredCheckStore {
    fn workload(&self) -> &'static str {
        "required_check"
    }

    async fn prune_batch_erased(&self, cutoff: OffsetDateTime) -> Result<u64, StorePruneFailure> {
        RequiredCheckStore::prune_batch(self, cutoff)
            .await
            .map_err(|_: RequiredCheckStoreError| StorePruneFailure)
    }
}

#[async_trait]
impl PrunableStore for WorkflowJobLinkStore {
    fn workload(&self) -> &'static str {
        "workflow_job_link"
    }

    async fn prune_batch_erased(&self, cutoff: OffsetDateTime) -> Result<u64, StorePruneFailure> {
        WorkflowJobLinkStore::prune_batch(self, cutoff)
            .await
            .map_err(|_: WorkflowJobLinkStoreError| StorePruneFailure)
    }
}

async fn prune_store(
    store: &dyn PrunableStore,
    cutoff: Option<OffsetDateTime>,
    shutdown: &watch::Receiver<bool>,
) -> StorePruneOutcome {
    let workload = store.workload();
    let Some(cutoff) = cutoff else {
        warn!(
            parent: None,
            workload,
            outcome = "invalid_cutoff",
            "retention pass skipped"
        );
        return StorePruneOutcome::Failed;
    };
    let mut batches = 0_u64;
    let mut deleted = 0_u64;

    loop {
        if *shutdown.borrow() {
            info!(
                parent: None,
                workload,
                outcome = "cancelled",
                batches,
                deleted,
                "retention pass stopped"
            );
            return StorePruneOutcome::Cancelled;
        }
        match store.prune_batch_erased(cutoff).await {
            Ok(batch_deleted) => {
                batches += 1;
                deleted = deleted.saturating_add(batch_deleted);
                if batch_deleted < FULL_PRUNE_BATCH_SIZE {
                    info!(
                        parent: None,
                        workload,
                        outcome = "completed",
                        batches,
                        deleted,
                        "retention pass finished"
                    );
                    return StorePruneOutcome::Completed;
                }
            }
            Err(_error) => {
                let error_correlation_id = ErrorCorrelationId::new();
                warn!(
                    parent: None,
                    workload,
                    outcome = "failed",
                    %error_correlation_id,
                    "retention pass failed"
                );
                return StorePruneOutcome::Failed;
            }
        }
    }
}

#[cfg(test)]
async fn prune_expired_deliveries(
    store: &DeliveryStore,
    retention: time::Duration,
    shutdown: &watch::Receiver<bool>,
) {
    prune_store(
        store,
        OffsetDateTime::now_utc().checked_sub(retention),
        shutdown,
    )
    .await;
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
        time::Duration,
    };

    use sqlx::{Row, SqlitePool};
    use tokio::sync::watch;
    use tracing::instrument::WithSubscriber;
    use tracing_subscriber::fmt::MakeWriter;

    use crate::storage::{open_database, DeliveryStore, MergeQueueStore};

    use super::{
        prune_expired_deliveries, run_retention, run_retention_once, RetentionConfig,
        RetentionError, RetentionPassOutcome, StorePruneOutcome,
    };

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    impl CapturedLogs {
        fn text(&self) -> String {
            let bytes = self.0.lock().expect("captured logs lock is available");
            String::from_utf8(bytes.clone()).expect("captured logs are UTF-8")
        }
    }

    struct CapturedLogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedLogWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("captured logs lock was poisoned"))?
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for CapturedLogs {
        type Writer = CapturedLogWriter;

        fn make_writer(&'writer self) -> Self::Writer {
            CapturedLogWriter(Arc::clone(&self.0))
        }
    }

    #[test]
    fn store_outcomes_map_to_bounded_pass_outcomes() {
        for (store_outcome, expected) in [
            (StorePruneOutcome::Completed, RetentionPassOutcome::Success),
            (
                StorePruneOutcome::Cancelled,
                RetentionPassOutcome::Cancelled,
            ),
            (StorePruneOutcome::Failed, RetentionPassOutcome::Failure),
        ] {
            assert_eq!(
                RetentionPassOutcome::Success.combine(store_outcome),
                expected
            );
        }
    }

    #[test]
    fn combined_pass_outcomes_prioritize_failure_then_cancellation() {
        for (current, store_outcome, expected) in [
            (
                RetentionPassOutcome::Success,
                StorePruneOutcome::Completed,
                RetentionPassOutcome::Success,
            ),
            (
                RetentionPassOutcome::Success,
                StorePruneOutcome::Cancelled,
                RetentionPassOutcome::Cancelled,
            ),
            (
                RetentionPassOutcome::Success,
                StorePruneOutcome::Failed,
                RetentionPassOutcome::Failure,
            ),
            (
                RetentionPassOutcome::Cancelled,
                StorePruneOutcome::Completed,
                RetentionPassOutcome::Cancelled,
            ),
            (
                RetentionPassOutcome::Cancelled,
                StorePruneOutcome::Cancelled,
                RetentionPassOutcome::Cancelled,
            ),
            (
                RetentionPassOutcome::Cancelled,
                StorePruneOutcome::Failed,
                RetentionPassOutcome::Failure,
            ),
            (
                RetentionPassOutcome::Failure,
                StorePruneOutcome::Completed,
                RetentionPassOutcome::Failure,
            ),
            (
                RetentionPassOutcome::Failure,
                StorePruneOutcome::Cancelled,
                RetentionPassOutcome::Failure,
            ),
            (
                RetentionPassOutcome::Failure,
                StorePruneOutcome::Failed,
                RetentionPassOutcome::Failure,
            ),
        ] {
            assert_eq!(current.combine(store_outcome), expected);
        }
    }

    async fn retention_stores() -> (
        tempfile::TempDir,
        SqlitePool,
        DeliveryStore,
        MergeQueueStore,
    ) {
        let directory = tempfile::tempdir().expect("temporary directory is created");
        let pool = open_database(&directory.path().join("retention.db"))
            .await
            .expect("database opens and migrates");
        sqlx::query(
            "INSERT INTO repositories (id, full_name, webhook_secret_ciphertext, \
             webhook_secret_nonce, encryption_version, enabled, created_at, updated_at) \
             VALUES (1, 'owner/repository', X'01', X'02', 1, 1, \
                     '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z')",
        )
        .execute(&pool)
        .await
        .expect("repository fixture is inserted");
        (
            directory,
            pool.clone(),
            DeliveryStore::new(pool.clone()),
            MergeQueueStore::new(pool),
        )
    }

    async fn delivery_count(pool: &SqlitePool) -> i64 {
        sqlx::query("SELECT COUNT(*) AS count FROM processed_deliveries")
            .fetch_one(pool)
            .await
            .expect("delivery claims are countable")
            .get("count")
    }

    async fn queue_attempt_count(pool: &SqlitePool) -> i64 {
        sqlx::query("SELECT COUNT(*) AS count FROM merge_queue_attempts")
            .fetch_one(pool)
            .await
            .expect("queue attempts are countable")
            .get("count")
    }

    async fn insert_queue_retention_fixtures(pool: &SqlitePool) {
        sqlx::query(
            "WITH RECURSIVE sequence(value) AS (\
                 VALUES(1) UNION ALL SELECT value + 1 FROM sequence WHERE value < 1005\
             )\
             INSERT INTO merge_queue_attempts \
                 (repository_id, pull_request_number, enqueued_at, completed_at, outcome, reason_code)\
             SELECT 1, value, '2020-01-01T00:00:00.000Z', '2020-01-02T00:00:00.000Z',\
                    'unknown', 'unclassified_dequeue' FROM sequence",
        )
        .execute(pool)
        .await
        .expect("expired queue attempts are inserted");
        sqlx::query(
            "INSERT INTO merge_queue_attempts \
                 (repository_id, pull_request_number, enqueued_at, completed_at, outcome, reason_code) \
             VALUES (1, 2001, '9998-01-01T00:00:00.000Z', '9999-01-01T00:00:00.000Z', \
                     'succeeded', 'pull_request_merged'), \
                    (1, 2002, '2020-01-01T00:00:00.000Z', NULL, 'pending', 'none')",
        )
        .execute(pool)
        .await
        .expect("retained queue attempts are inserted");
    }

    #[test]
    fn configuration_rejects_zero_and_unrepresentable_durations() {
        assert_eq!(
            RetentionConfig::new(
                Duration::ZERO,
                Duration::from_secs(1),
                Duration::from_secs(1),
            ),
            Err(RetentionError::InvalidInterval)
        );
        for (delivery_retention, merge_queue_retention) in [
            (Duration::ZERO, Duration::from_secs(1)),
            (Duration::MAX, Duration::from_secs(1)),
            (Duration::from_secs(1), Duration::ZERO),
            (Duration::from_secs(1), Duration::MAX),
        ] {
            assert_eq!(
                RetentionConfig::new(
                    Duration::from_secs(1),
                    delivery_retention,
                    merge_queue_retention,
                ),
                Err(RetentionError::InvalidRetention)
            );
        }
    }

    #[tokio::test]
    async fn prune_failure_is_redacted_and_carries_a_correlation_id() {
        let (_directory, pool, store, _queue_store) = retention_stores().await;
        sqlx::query("DROP TABLE processed_deliveries")
            .execute(&pool)
            .await
            .expect("delivery table is removed");
        let (_shutdown_sender, shutdown_receiver) = watch::channel(false);
        let captured_logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(captured_logs.clone())
            .finish();

        prune_expired_deliveries(&store, time::Duration::days(1), &shutdown_receiver)
            .with_subscriber(subscriber)
            .await;

        let logs = captured_logs.text();
        assert!(logs.contains("outcome=\"failed\""));
        let correlation_id = logs
            .split("error_correlation_id=")
            .nth(1)
            .and_then(|suffix| suffix.split_whitespace().next())
            .expect("failure log includes a correlation ID")
            .trim_matches('"');
        uuid::Uuid::parse_str(correlation_id).expect("correlation ID is an opaque UUID");
        for forbidden in ["processed_deliveries", "no such table", "SqliteError"] {
            assert!(!logs.contains(forbidden));
        }
    }

    #[tokio::test]
    async fn interval_prunes_all_expired_batches_but_preserves_fresh_claims() {
        let (_directory, pool, store, queue_store) = retention_stores().await;
        sqlx::query(
            "WITH RECURSIVE sequence(value) AS (\
                 VALUES(1) UNION ALL SELECT value + 1 FROM sequence WHERE value < 1005\
             )\
             INSERT INTO processed_deliveries (delivery_id, received_at)\
             SELECT printf('00000000-0000-4000-8000-%012d', value),\
                    '2020-01-01T00:00:00.000Z'\
             FROM sequence",
        )
        .execute(&pool)
        .await
        .expect("expired claims are inserted");
        sqlx::query(
            "INSERT INTO processed_deliveries (delivery_id, received_at) VALUES\
             ('10000000-0000-4000-8000-000000000001', '9999-01-01T00:00:00.000Z'),\
             ('10000000-0000-4000-8000-000000000002', '9999-01-01T00:00:00.000Z')",
        )
        .execute(&pool)
        .await
        .expect("fresh claims are inserted");
        assert_eq!(delivery_count(&pool).await, 1_007);
        tokio::time::pause();
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        let config = RetentionConfig::new(
            Duration::from_secs(60),
            Duration::from_secs(86_400),
            Duration::from_secs(90 * 86_400),
        )
        .expect("retention configuration is valid");
        let runner = tokio::spawn(run_retention(store, queue_store, config, shutdown_receiver));

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::time::resume();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while delivery_count(&pool).await != 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "retention pass did not finish before the test deadline"
            );
            tokio::task::yield_now().await;
        }

        shutdown_sender
            .send(true)
            .expect("retention runner receives shutdown");
        runner.await.expect("retention runner joins");
        assert_eq!(delivery_count(&pool).await, 2);
    }

    #[tokio::test]
    async fn cancellation_prevents_another_scheduled_prune() {
        let (_directory, pool, store, queue_store) = retention_stores().await;
        insert_queue_retention_fixtures(&pool).await;
        sqlx::query(
            "INSERT INTO processed_deliveries (delivery_id, received_at) VALUES\
             ('20000000-0000-4000-8000-000000000001', '2020-01-01T00:00:00.000Z')",
        )
        .execute(&pool)
        .await
        .expect("expired claim is inserted");
        tokio::time::pause();
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        let config = RetentionConfig::new(
            Duration::from_secs(60),
            Duration::from_secs(86_400),
            Duration::from_secs(90 * 86_400),
        )
        .expect("retention configuration is valid");
        let runner = tokio::spawn(run_retention(store, queue_store, config, shutdown_receiver));

        shutdown_sender
            .send(true)
            .expect("retention runner receives shutdown");
        tokio::time::advance(Duration::from_secs(120)).await;
        runner.await.expect("retention runner joins");
        tokio::time::resume();

        assert_eq!(delivery_count(&pool).await, 1);
        assert_eq!(queue_attempt_count(&pool).await, 1_007);
    }

    #[tokio::test]
    async fn interval_prunes_all_expired_queue_batches_but_preserves_pending_and_fresh_attempts() {
        let (_directory, pool, delivery_store, queue_store) = retention_stores().await;
        insert_queue_retention_fixtures(&pool).await;
        assert_eq!(queue_attempt_count(&pool).await, 1_007);
        tokio::time::pause();
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        let config = RetentionConfig::new(
            Duration::from_secs(60),
            Duration::from_secs(86_400),
            Duration::from_secs(90 * 86_400),
        )
        .expect("retention configuration is valid");
        let runner = tokio::spawn(run_retention(
            delivery_store,
            queue_store,
            config,
            shutdown_receiver,
        ));

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::time::resume();
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while queue_attempt_count(&pool).await != 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "queue retention pass did not finish before the test deadline"
            );
            tokio::task::yield_now().await;
        }

        shutdown_sender
            .send(true)
            .expect("retention runner receives shutdown");
        runner.await.expect("retention runner joins");
        let retained_numbers: Vec<i64> = sqlx::query_scalar(
            "SELECT pull_request_number FROM merge_queue_attempts ORDER BY pull_request_number",
        )
        .fetch_all(&pool)
        .await
        .expect("retained attempts are readable");
        assert_eq!(retained_numbers, vec![2001, 2002]);
    }

    #[tokio::test]
    async fn retention_prunes_workflow_context_with_the_delivery_cutoff() {
        let (_directory, pool, delivery_store, queue_store) = retention_stores().await;
        sqlx::query(
            "INSERT INTO workflow_run_contexts \
             (repository_id, workflow_run_id, workflow_run_attempt, event, updated_at) \
             VALUES (1, 31, 1, 'pull_request', '2020-01-01T00:00:00.000Z')",
        )
        .execute(&pool)
        .await
        .expect("expired workflow context inserts");
        let (_shutdown_sender, shutdown_receiver) = watch::channel(false);
        let config = RetentionConfig::new(
            Duration::from_millis(10),
            Duration::from_secs(86_400),
            Duration::from_secs(90 * 86_400),
        )
        .expect("retention configuration is valid");

        run_retention_once(&delivery_store, &queue_store, config, &shutdown_receiver).await;

        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM workflow_run_contexts")
            .fetch_one(&pool)
            .await
            .expect("workflow contexts are countable");
        assert_eq!(count, 0);
    }

    #[tokio::test]
    async fn delivery_failure_does_not_prevent_queue_pruning_in_the_same_pass() {
        let (_directory, pool, delivery_store, queue_store) = retention_stores().await;
        insert_queue_retention_fixtures(&pool).await;
        sqlx::query("DROP TABLE processed_deliveries")
            .execute(&pool)
            .await
            .expect("delivery table is removed");
        let (_shutdown_sender, shutdown_receiver) = watch::channel(false);
        let config = RetentionConfig::new(
            Duration::from_millis(10),
            Duration::from_secs(86_400),
            Duration::from_secs(90 * 86_400),
        )
        .expect("retention configuration is valid");

        run_retention_once(&delivery_store, &queue_store, config, &shutdown_receiver).await;

        assert_eq!(queue_attempt_count(&pool).await, 2);
    }

    #[tokio::test]
    async fn queue_failure_is_redacted_correlated_and_recovers_at_the_next_interval() {
        let (_directory, pool, delivery_store, queue_store) = retention_stores().await;
        insert_queue_retention_fixtures(&pool).await;
        sqlx::query(
            "CREATE TRIGGER reject_queue_prune BEFORE DELETE ON merge_queue_attempts \
             BEGIN SELECT RAISE(ABORT, 'sensitive-queue-prune-failure'); END",
        )
        .execute(&pool)
        .await
        .expect("queue prune failure trigger is installed");
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        let captured_logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(captured_logs.clone())
            .finish();
        let config = RetentionConfig::new(
            Duration::from_millis(10),
            Duration::from_secs(86_400),
            Duration::from_secs(90 * 86_400),
        )
        .expect("retention configuration is valid");
        let runner = tokio::spawn(
            run_retention(delivery_store, queue_store, config, shutdown_receiver)
                .with_subscriber(subscriber),
        );

        let failure_deadline = std::time::Instant::now() + Duration::from_secs(1);
        while !captured_logs.text().contains("outcome=\"failed\"") {
            assert!(
                std::time::Instant::now() < failure_deadline,
                "queue retention failure was not logged before the test deadline"
            );
            tokio::task::yield_now().await;
        }
        assert_eq!(queue_attempt_count(&pool).await, 1_007);
        sqlx::query("DROP TRIGGER reject_queue_prune")
            .execute(&pool)
            .await
            .expect("queue prune failure trigger is removed");
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while queue_attempt_count(&pool).await != 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "recovered queue retention pass did not finish before the test deadline"
            );
            tokio::task::yield_now().await;
        }
        shutdown_sender
            .send(true)
            .expect("retention runner receives shutdown");
        runner.await.expect("retention runner joins");

        let logs = captured_logs.text();
        assert!(logs.contains("workload=\"merge_queue\""));
        assert!(logs.contains("outcome=\"failed\""));
        let correlation_id = logs
            .split("error_correlation_id=")
            .nth(1)
            .and_then(|suffix| suffix.split_whitespace().next())
            .expect("failure log includes a correlation ID")
            .trim_matches('"');
        uuid::Uuid::parse_str(correlation_id).expect("correlation ID is an opaque UUID");
        for forbidden in [
            "merge_queue_attempts",
            "reject_queue_prune",
            "sensitive-queue-prune-failure",
            "SqliteError",
        ] {
            assert!(!logs.contains(forbidden));
        }
    }
}
