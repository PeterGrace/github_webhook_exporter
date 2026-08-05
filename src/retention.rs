use std::time::Duration;

use thiserror::Error;
use time::OffsetDateTime;
use tokio::{sync::watch, time::Instant};
use tracing::{info, warn};

use crate::{error::ErrorCorrelationId, storage::DeliveryStore};

const FULL_PRUNE_BATCH_SIZE: u64 = 1_000;

/// Validated scheduling and age limits for processed-delivery retention.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionConfig {
    interval: Duration,
    retention: time::Duration,
}

impl RetentionConfig {
    /// Validates a positive prune interval and a retention duration representable by `time`.
    ///
    /// # Errors
    ///
    /// Returns [`RetentionError::InvalidInterval`] for a zero interval and
    /// [`RetentionError::InvalidRetention`] when the retention duration is zero or cannot be
    /// represented safely for cutoff calculation.
    pub fn new(interval: Duration, retention: Duration) -> Result<Self, RetentionError> {
        if interval.is_zero() {
            return Err(RetentionError::InvalidInterval);
        }
        if retention.is_zero() {
            return Err(RetentionError::InvalidRetention);
        }
        let retention =
            time::Duration::try_from(retention).map_err(|_| RetentionError::InvalidRetention)?;
        Ok(Self {
            interval,
            retention,
        })
    }
}

/// A stable delivery-retention configuration failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum RetentionError {
    /// The scheduling interval was zero.
    #[error("delivery prune interval must be positive")]
    InvalidInterval,
    /// The retention duration was zero or outside the supported range.
    #[error("delivery retention duration is invalid")]
    InvalidRetention,
}

/// Runs scheduled bounded delivery pruning until shared lifecycle cancellation is requested.
///
/// The first pass starts only after one full interval. A shutdown received during an active SQLite
/// batch allows that batch to finish, then prevents another batch from starting.
pub async fn run_delivery_retention(
    store: DeliveryStore,
    config: RetentionConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let start = Instant::now() + config.interval;
    let mut ticker = tokio::time::interval_at(start, config.interval);

    loop {
        tokio::select! {
            biased;
            () = wait_for_shutdown(&mut shutdown) => return,
            _ = ticker.tick() => prune_expired_deliveries(&store, config.retention, &shutdown).await,
        }
    }
}

async fn prune_expired_deliveries(
    store: &DeliveryStore,
    retention: time::Duration,
    shutdown: &watch::Receiver<bool>,
) {
    let Some(cutoff) = OffsetDateTime::now_utc().checked_sub(retention) else {
        warn!(
            outcome = "invalid_cutoff",
            "delivery retention pass skipped"
        );
        return;
    };
    let mut batches = 0_u64;
    let mut deleted = 0_u64;

    loop {
        if *shutdown.borrow() {
            info!(
                outcome = "cancelled",
                batches, deleted, "delivery retention pass stopped"
            );
            return;
        }
        match store.prune_batch(cutoff).await {
            Ok(batch_deleted) => {
                batches += 1;
                deleted = deleted.saturating_add(batch_deleted);
                if batch_deleted < FULL_PRUNE_BATCH_SIZE {
                    info!(
                        outcome = "completed",
                        batches, deleted, "delivery retention pass finished"
                    );
                    return;
                }
            }
            Err(error) => {
                let error_correlation_id = ErrorCorrelationId::new();
                warn!(
                    outcome = "failed",
                    error = %error,
                    %error_correlation_id,
                    "delivery retention pass failed"
                );
                return;
            }
        }
    }
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
    use std::time::Duration;

    use sqlx::{Row, SqlitePool};
    use tokio::sync::watch;

    use crate::storage::{open_database, DeliveryStore};

    use super::{run_delivery_retention, RetentionConfig};

    async fn retention_store() -> (tempfile::TempDir, SqlitePool, DeliveryStore) {
        let directory = tempfile::tempdir().expect("temporary directory is created");
        let pool = open_database(&directory.path().join("retention.db"))
            .await
            .expect("database opens and migrates");
        let store = DeliveryStore::new(pool.clone());
        (directory, pool, store)
    }

    async fn delivery_count(pool: &SqlitePool) -> i64 {
        sqlx::query("SELECT COUNT(*) AS count FROM processed_deliveries")
            .fetch_one(pool)
            .await
            .expect("delivery claims are countable")
            .get("count")
    }

    #[tokio::test]
    async fn interval_prunes_all_expired_batches_but_preserves_fresh_claims() {
        let (_directory, pool, store) = retention_store().await;
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
        let config = RetentionConfig::new(Duration::from_secs(60), Duration::from_secs(86_400))
            .expect("retention configuration is valid");
        let runner = tokio::spawn(run_delivery_retention(store, config, shutdown_receiver));

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
        let (_directory, pool, store) = retention_store().await;
        sqlx::query(
            "INSERT INTO processed_deliveries (delivery_id, received_at) VALUES\
             ('20000000-0000-4000-8000-000000000001', '2020-01-01T00:00:00.000Z')",
        )
        .execute(&pool)
        .await
        .expect("expired claim is inserted");
        tokio::time::pause();
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        let config = RetentionConfig::new(Duration::from_secs(60), Duration::from_secs(86_400))
            .expect("retention configuration is valid");
        let runner = tokio::spawn(run_delivery_retention(store, config, shutdown_receiver));

        shutdown_sender
            .send(true)
            .expect("retention runner receives shutdown");
        tokio::time::advance(Duration::from_secs(120)).await;
        runner.await.expect("retention runner joins");
        tokio::time::resume();

        assert_eq!(delivery_count(&pool).await, 1);
    }
}
