use std::fmt;

use sqlx::SqlitePool;
use thiserror::Error;
use time::OffsetDateTime;

use crate::domain::{
    merge_queue::{
        PullRequestNumber, QueueCompletion, QueueOutcome, QueueReasonCode, QueueTimestamp,
    },
    repository::RepositoryId,
};

use super::sqlite_is_busy_or_locked;

const ATTEMPT_PRUNE_BATCH_SIZE: i64 = 1_000;

/// Whether enqueue created a durable attempt or found the existing active attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EnqueueTransition {
    /// A new pending attempt was persisted.
    Created,
    /// An active attempt already existed, so persisted state did not change.
    AlreadyActive,
}

/// The result of trying to complete one durable merge-queue attempt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CompletionTransition {
    /// One pending attempt changed to the requested terminal state.
    Completed {
        /// The committed attempt's enqueue timestamp, used for bounded duration metrics.
        enqueued_at: QueueTimestamp,
    },
    /// A terminal attempt already existed, so persisted state did not change.
    AlreadyCompleted,
    /// No pending or terminal attempt exists for the repository and pull request.
    MissingActiveAttempt,
}

/// Transactional persistence for durable merge-queue attempts.
#[derive(Clone)]
pub struct MergeQueueStore {
    pool: SqlitePool,
}

impl MergeQueueStore {
    /// Creates a merge-queue store from an already migrated SQLite pool.
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Atomically creates a pending attempt unless one is already active.
    ///
    /// `repository_id` must identify an existing repository. `pull_request_number` and
    /// `enqueued_at` are validated domain values, preventing invalid persistence inputs.
    ///
    /// # Errors
    ///
    /// Returns [`MergeQueueStoreError::Unavailable`] when SQLite is busy or locked and
    /// [`MergeQueueStoreError::Internal`] for all other persistence failures, including an unknown
    /// repository identifier.
    pub async fn enqueue(
        &self,
        repository_id: RepositoryId,
        pull_request_number: PullRequestNumber,
        enqueued_at: &QueueTimestamp,
    ) -> Result<EnqueueTransition, MergeQueueStoreError> {
        let mut transaction = self.pool.begin().await.map_err(map_sqlx_error)?;
        let result = sqlx::query(
            "INSERT INTO merge_queue_attempts \
             (repository_id, pull_request_number, enqueued_at, completed_at, outcome, reason_code) \
             VALUES (?, ?, ?, NULL, ?, ?) \
             ON CONFLICT(repository_id, pull_request_number) WHERE completed_at IS NULL \
             DO NOTHING",
        )
        .bind(repository_id.get())
        .bind(pull_request_number.get())
        .bind(enqueued_at.as_str())
        .bind(QueueOutcome::Pending.as_str())
        .bind(QueueReasonCode::None.as_str())
        .execute(&mut *transaction)
        .await
        .map_err(map_sqlx_error)?;
        transaction.commit().await.map_err(map_sqlx_error)?;

        Ok(if result.rows_affected() == 1 {
            EnqueueTransition::Created
        } else {
            EnqueueTransition::AlreadyActive
        })
    }

    /// Atomically completes an active attempt or reports the exact expected no-op state.
    ///
    /// The validated `completion` boundary permits only Phase 3's evidence-backed merged success
    /// and deliberately unclassified dequeue transition.
    ///
    /// # Errors
    ///
    /// Returns [`MergeQueueStoreError::Unavailable`] when SQLite is busy or locked and
    /// [`MergeQueueStoreError::Internal`] for every other persistence or transaction failure.
    pub async fn complete(
        &self,
        repository_id: RepositoryId,
        pull_request_number: PullRequestNumber,
        completion: &QueueCompletion,
    ) -> Result<CompletionTransition, MergeQueueStoreError> {
        let mut transaction = self.pool.begin().await.map_err(map_sqlx_error)?;
        let enqueued_at: Option<String> = sqlx::query_scalar(
            "UPDATE merge_queue_attempts \
             SET completed_at = ?, outcome = ?, reason_code = ? \
             WHERE repository_id = ? AND pull_request_number = ? AND completed_at IS NULL \
             RETURNING enqueued_at",
        )
        .bind(completion.completed_at().as_str())
        .bind(completion.outcome().as_str())
        .bind(completion.reason_code().as_str())
        .bind(repository_id.get())
        .bind(pull_request_number.get())
        .fetch_optional(&mut *transaction)
        .await
        .map_err(map_sqlx_error)?;

        let transition = if let Some(enqueued_at) = enqueued_at {
            CompletionTransition::Completed {
                enqueued_at: QueueTimestamp::parse(&enqueued_at)
                    .map_err(|_| MergeQueueStoreError::Internal)?,
            }
        } else {
            let completed_exists: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM merge_queue_attempts \
                 WHERE repository_id = ? AND pull_request_number = ? AND completed_at IS NOT NULL)",
            )
            .bind(repository_id.get())
            .bind(pull_request_number.get())
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx_error)?;
            if completed_exists {
                CompletionTransition::AlreadyCompleted
            } else {
                CompletionTransition::MissingActiveAttempt
            }
        };
        transaction.commit().await.map_err(map_sqlx_error)?;
        Ok(transition)
    }

    /// Deletes at most 1,000 completed attempts older than `cutoff`.
    ///
    /// Pending attempts are never eligible. Callers retain scheduling and cancellation control by
    /// repeating this bounded operation while it returns 1,000.
    ///
    /// # Errors
    ///
    /// Returns [`MergeQueueStoreError::Unavailable`] when SQLite is busy or locked and
    /// [`MergeQueueStoreError::Internal`] for timestamp formatting or other persistence failures.
    pub async fn prune_completed_batch(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<u64, MergeQueueStoreError> {
        let cutoff =
            QueueTimestamp::from_datetime(cutoff).map_err(|_| MergeQueueStoreError::Internal)?;
        // Text ordering is chronological because every store write uses QueueTimestamp's
        // fixed-width UTC millisecond representation. Do not admit alternate timestamp formats.
        let result = sqlx::query(
            "DELETE FROM merge_queue_attempts \
             WHERE id IN (\
                 SELECT id FROM merge_queue_attempts \
                 WHERE completed_at IS NOT NULL AND completed_at < ? \
                 ORDER BY completed_at, id LIMIT ?\
             )",
        )
        .bind(cutoff.as_str())
        .bind(ATTEMPT_PRUNE_BATCH_SIZE)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(result.rows_affected())
    }
}

/// A stable, redacted merge-queue persistence failure.
#[derive(Clone, Copy, Error, PartialEq, Eq)]
pub enum MergeQueueStoreError {
    /// SQLite is temporarily busy or locked.
    #[error("merge-queue storage is temporarily unavailable")]
    Unavailable,
    /// SQLite returned an unexpected persistence failure whose details were discarded.
    #[error("internal merge-queue persistence failure")]
    Internal,
}

impl fmt::Debug for MergeQueueStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "MergeQueueStoreError::Unavailable",
            Self::Internal => "MergeQueueStoreError::Internal",
        })
    }
}

fn map_sqlx_error(error: sqlx::Error) -> MergeQueueStoreError {
    if sqlite_is_busy_or_locked(&error) {
        MergeQueueStoreError::Unavailable
    } else {
        MergeQueueStoreError::Internal
    }
}
