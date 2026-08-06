use std::fmt;

use crate::{
    domain::delivery::DeliveryId,
    telemetry::trace::{self, DatabaseOperation},
};
use sqlx::SqlitePool;
use thiserror::Error;
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime, UtcOffset};

use super::sqlite_is_busy_or_locked;

const DELIVERY_PRUNE_BATCH_SIZE: i64 = 1_000;
const DELIVERY_TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// Whether an atomic delivery claim inserted a new row or found an existing claim.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeliveryClaim {
    /// The delivery was claimed for the first time.
    New,
    /// The delivery had already been claimed.
    Duplicate,
}

/// Durable persistence for authenticated GitHub webhook delivery claims.
#[derive(Clone)]
pub struct DeliveryStore {
    pool: SqlitePool,
}

impl DeliveryStore {
    /// Creates a delivery store from an already migrated SQLite pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Atomically claims a delivery identifier without changing an existing receipt time.
    ///
    /// A committed claim is durable across restarts. A process crash after this method returns
    /// [`DeliveryClaim::New`] but before the caller updates metrics can undercount that delivery;
    /// this boundary does not provide exactly-once metric updates across crashes.
    ///
    /// # Errors
    ///
    /// Returns [`DeliveryStoreError::Unavailable`] when SQLite is busy or locked and
    /// [`DeliveryStoreError::Internal`] for other persistence failures.
    pub async fn claim(
        &self,
        delivery_id: &DeliveryId,
    ) -> Result<DeliveryClaim, DeliveryStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::DeliveryClaim,
            self.claim_inner(delivery_id),
        )
        .await
    }

    async fn claim_inner(
        &self,
        delivery_id: &DeliveryId,
    ) -> Result<DeliveryClaim, DeliveryStoreError> {
        let mut buffer = uuid::Uuid::encode_buffer();
        let result = sqlx::query(
            "INSERT INTO processed_deliveries (delivery_id, received_at) \
             VALUES (?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) \
             ON CONFLICT(delivery_id) DO NOTHING",
        )
        .bind(delivery_id.encode_lower(&mut buffer))
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        Ok(if result.rows_affected() == 1 {
            DeliveryClaim::New
        } else {
            DeliveryClaim::Duplicate
        })
    }

    /// Deletes at most 1,000 delivery claims received before `cutoff`.
    ///
    /// Callers can repeat this operation while it returns 1,000 and check for lifecycle
    /// cancellation between calls. Keeping the loop outside this store prevents one operation
    /// from monopolizing SQLite or delaying shutdown.
    ///
    /// # Errors
    ///
    /// Returns [`DeliveryStoreError::Unavailable`] when SQLite is busy or locked and
    /// [`DeliveryStoreError::Internal`] for other persistence failures.
    pub async fn prune_batch(&self, cutoff: OffsetDateTime) -> Result<u64, DeliveryStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::DeliveryPrune,
            self.prune_batch_inner(cutoff),
        )
        .await
    }

    async fn prune_batch_inner(&self, cutoff: OffsetDateTime) -> Result<u64, DeliveryStoreError> {
        let cutoff = cutoff
            .to_offset(UtcOffset::UTC)
            .format(DELIVERY_TIMESTAMP_FORMAT)
            .map_err(|_| DeliveryStoreError::Internal)?;
        let result = sqlx::query(
            "DELETE FROM processed_deliveries \
             WHERE delivery_id IN (\
                 SELECT delivery_id FROM processed_deliveries \
                 WHERE received_at < ? \
                 ORDER BY received_at, delivery_id \
                 LIMIT ?\
             )",
        )
        .bind(cutoff)
        .bind(DELIVERY_PRUNE_BATCH_SIZE)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        Ok(result.rows_affected())
    }
}

/// A stable, redacted delivery-claim persistence failure.
#[derive(Clone, Copy, Error, PartialEq, Eq)]
pub enum DeliveryStoreError {
    /// SQLite is temporarily busy or locked.
    #[error("delivery storage is temporarily unavailable")]
    Unavailable,
    /// SQLite returned an unexpected persistence failure whose details were discarded.
    #[error("internal delivery persistence failure")]
    Internal,
}

impl fmt::Debug for DeliveryStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "DeliveryStoreError::Unavailable",
            Self::Internal => "DeliveryStoreError::Internal",
        })
    }
}

fn map_sqlx_error(error: sqlx::Error) -> DeliveryStoreError {
    if sqlite_is_busy_or_locked(&error) {
        DeliveryStoreError::Unavailable
    } else {
        DeliveryStoreError::Internal
    }
}
