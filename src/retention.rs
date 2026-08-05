use std::{future::Future, time::Duration};

use thiserror::Error;
use time::OffsetDateTime;
use tokio::{sync::watch, time::Instant};
use tracing::{info, warn};

use crate::{
    error::ErrorCorrelationId,
    storage::{DeliveryStore, DeliveryStoreError, MergeQueueStore, MergeQueueStoreError},
};

const FULL_PRUNE_BATCH_SIZE: u64 = 1_000;

/// Validated scheduling and age limits for delivery and merge-queue retention.
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

/// Runs scheduled bounded delivery and merge-queue pruning until cancellation is requested.
///
/// The first pass starts only after one full interval, and missed ticks are skipped. A shutdown
/// received during an active SQLite batch allows that batch to finish, then prevents another batch
/// from starting. Both workloads use cutoffs fixed at the beginning of each scheduled pass.
pub async fn run_retention(
    delivery_store: DeliveryStore,
    merge_queue_store: MergeQueueStore,
    config: RetentionConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let start = Instant::now() + config.interval;
    let mut ticker = tokio::time::interval_at(start, config.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            biased;
            () = wait_for_shutdown(&mut shutdown) => return,
            _ = ticker.tick() => {
                prune_retention_pass(
                    &delivery_store,
                    &merge_queue_store,
                    config,
                    &shutdown,
                ).await;
            }
        }
    }
}

async fn prune_retention_pass(
    delivery_store: &DeliveryStore,
    merge_queue_store: &MergeQueueStore,
    config: RetentionConfig,
    shutdown: &watch::Receiver<bool>,
) {
    let pass_started_at = OffsetDateTime::now_utc();
    prune_store(
        delivery_store,
        pass_started_at.checked_sub(config.delivery_retention),
        shutdown,
    )
    .await;
    if *shutdown.borrow() {
        return;
    }
    prune_store(
        merge_queue_store,
        pass_started_at.checked_sub(config.merge_queue_retention),
        shutdown,
    )
    .await;
}

trait PrunableStore {
    type Error;

    const WORKLOAD: &'static str;

    fn prune_batch(
        &self,
        cutoff: OffsetDateTime,
    ) -> impl Future<Output = Result<u64, Self::Error>> + Send;
}

impl PrunableStore for DeliveryStore {
    type Error = DeliveryStoreError;

    const WORKLOAD: &'static str = "delivery";

    async fn prune_batch(&self, cutoff: OffsetDateTime) -> Result<u64, Self::Error> {
        DeliveryStore::prune_batch(self, cutoff).await
    }
}

impl PrunableStore for MergeQueueStore {
    type Error = MergeQueueStoreError;

    const WORKLOAD: &'static str = "merge_queue";

    async fn prune_batch(&self, cutoff: OffsetDateTime) -> Result<u64, Self::Error> {
        self.prune_completed_batch(cutoff).await
    }
}

async fn prune_store<S: PrunableStore>(
    store: &S,
    cutoff: Option<OffsetDateTime>,
    shutdown: &watch::Receiver<bool>,
) {
    let Some(cutoff) = cutoff else {
        warn!(
            workload = S::WORKLOAD,
            outcome = "invalid_cutoff",
            "retention pass skipped"
        );
        return;
    };
    let mut batches = 0_u64;
    let mut deleted = 0_u64;

    loop {
        if *shutdown.borrow() {
            info!(
                workload = S::WORKLOAD,
                outcome = "cancelled",
                batches,
                deleted,
                "retention pass stopped"
            );
            return;
        }
        match store.prune_batch(cutoff).await {
            Ok(batch_deleted) => {
                batches += 1;
                deleted = deleted.saturating_add(batch_deleted);
                if batch_deleted < FULL_PRUNE_BATCH_SIZE {
                    info!(
                        workload = S::WORKLOAD,
                        outcome = "completed",
                        batches,
                        deleted,
                        "retention pass finished"
                    );
                    return;
                }
            }
            Err(_error) => {
                let error_correlation_id = ErrorCorrelationId::new();
                warn!(
                    workload = S::WORKLOAD,
                    outcome = "failed",
                    %error_correlation_id,
                    "retention pass failed"
                );
                return;
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

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
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

    use super::{prune_expired_deliveries, run_retention, RetentionConfig, RetentionError};

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
    async fn delivery_failure_does_not_prevent_queue_pruning_in_the_same_pass() {
        let (_directory, pool, delivery_store, queue_store) = retention_stores().await;
        insert_queue_retention_fixtures(&pool).await;
        sqlx::query("DROP TABLE processed_deliveries")
            .execute(&pool)
            .await
            .expect("delivery table is removed");
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

        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while queue_attempt_count(&pool).await != 2 {
            assert!(
                std::time::Instant::now() < deadline,
                "queue pruning did not finish after the delivery failure"
            );
            tokio::task::yield_now().await;
        }
        shutdown_sender
            .send(true)
            .expect("retention runner receives shutdown");
        runner.await.expect("retention runner joins");

        let logs = captured_logs.text();
        assert!(logs.contains("workload=\"delivery\" outcome=\"failed\""));
        assert!(logs.contains("workload=\"merge_queue\" outcome=\"completed\""));
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
