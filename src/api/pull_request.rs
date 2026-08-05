use serde::Deserialize;
use time::OffsetDateTime;

use crate::{
    domain::{
        merge_queue::{PullRequestNumber, QueueCompletion, QueueTimestamp},
        repository::RepositoryId,
    },
    metrics::{Action, EventType, MergeQueueCompletion, Metrics, QueueTransitionFailureReason},
    storage::{CompletionTransition, MergeQueueStore, MergeQueueStoreError},
};

/// Minimal authenticated payload fields needed for pull-request queue processing.
#[derive(Deserialize)]
pub(super) struct PullRequestProjection {
    number: i64,
    updated_at: Option<String>,
    merged: Option<bool>,
}

/// Authenticated, bounded dependencies for one pull-request queue event.
pub(super) struct QueueProcessor<'a> {
    pub(super) repository_id: RepositoryId,
    pub(super) event_type: EventType,
    pub(super) action: Action,
    pub(super) received_at: OffsetDateTime,
    pub(super) store: &'a MergeQueueStore,
    pub(super) metrics: &'a Metrics,
}

impl QueueProcessor<'_> {
    /// Applies one supported pull-request queue transition.
    ///
    /// Unsupported actions and invalid pull-request numbers are specialized no-ops. Missing or
    /// invalid event timestamps use the request receipt timestamp.
    ///
    /// # Errors
    ///
    /// Returns a redacted persistence error when the durable queue transition cannot commit.
    pub(super) async fn process(
        &self,
        projection: &PullRequestProjection,
    ) -> Result<(), MergeQueueStoreError> {
        if self.event_type != EventType::PullRequest {
            return Ok(());
        }
        let Ok(pull_request_number) = PullRequestNumber::new(projection.number) else {
            return Ok(());
        };
        let event_timestamp = projection
            .updated_at
            .as_deref()
            .and_then(|value| QueueTimestamp::parse(value).ok())
            .or_else(|| QueueTimestamp::from_datetime(self.received_at).ok());
        let Some(event_timestamp) = event_timestamp else {
            // `received_at` comes from `OffsetDateTime::now_utc()` and therefore has a year that
            // the canonical queue timestamp format can represent. Keep this guard defensive in
            // case a future caller supplies a synthetic out-of-range receipt timestamp.
            debug_assert!(false, "request receipt timestamp must be representable");
            return Ok(());
        };

        match self.action {
            Action::Enqueued => {
                let _transition = self
                    .store
                    .enqueue(self.repository_id, pull_request_number, &event_timestamp)
                    .await?;
            }
            Action::Dequeued => {
                self.complete_attempt(
                    pull_request_number,
                    QueueCompletion::unclassified_dequeue(event_timestamp),
                    MergeQueueCompletion::UnclassifiedDequeue,
                )
                .await?;
            }
            Action::Closed if projection.merged == Some(true) => {
                self.complete_attempt(
                    pull_request_number,
                    QueueCompletion::pull_request_merged(event_timestamp),
                    MergeQueueCompletion::PullRequestMerged,
                )
                .await?;
            }
            Action::Assigned
            | Action::ChecksRequested
            | Action::Closed
            | Action::Completed
            | Action::Created
            | Action::Deleted
            | Action::Destroyed
            | Action::Edited
            | Action::InProgress
            | Action::Labeled
            | Action::Opened
            | Action::Published
            | Action::Queued
            | Action::Reopened
            | Action::Requested
            | Action::RequestedAction
            | Action::Rerequested
            | Action::Submitted
            | Action::Synchronize
            | Action::Unassigned
            | Action::Unlabeled
            | Action::Unpublished
            | Action::Updated
            | Action::Waiting
            | Action::None
            | Action::Other => {}
        }
        Ok(())
    }

    async fn complete_attempt(
        &self,
        pull_request_number: PullRequestNumber,
        completion: QueueCompletion,
        metric_completion: MergeQueueCompletion,
    ) -> Result<(), MergeQueueStoreError> {
        match self
            .store
            .complete(self.repository_id, pull_request_number, &completion)
            .await?
        {
            CompletionTransition::Completed { enqueued_at } => {
                self.metrics.record_merge_queue_completion(
                    metric_completion,
                    completion.completed_at().duration_since(&enqueued_at),
                );
            }
            CompletionTransition::AlreadyCompleted => {}
            CompletionTransition::MissingActiveAttempt => {
                self.metrics.record_merge_queue_transition_failure(
                    QueueTransitionFailureReason::MissingActiveAttempt,
                );
            }
        }
        Ok(())
    }
}
