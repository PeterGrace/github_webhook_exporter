use serde::Deserialize;
use serde_json::Value;
use time::OffsetDateTime;
use tracing::Instrument;

use crate::{
    domain::{
        delivery::DeliveryId,
        merge_queue::{PullRequestNumber, QueueCompletion, QueueTimestamp},
        repository::RepositoryId,
    },
    metrics::{Action, EventType, MergeQueueCompletion, Metrics, QueueTransitionFailureReason},
    security::CanonicalRepositoryName,
    storage::{CompletionTransition, EnqueueTransition, MergeQueueStore, MergeQueueStoreError},
    telemetry::trace::{
        self, CommitSha, Operation, OperationFailureReason, OperationOutcome, QueueEntity,
    },
};

#[derive(Deserialize)]
struct HeadProjection {
    sha: Option<Value>,
}

/// Minimal authenticated payload fields needed for pull-request queue processing.
#[derive(Deserialize)]
pub(super) struct PullRequestProjection {
    number: i64,
    updated_at: Option<String>,
    merged: Option<bool>,
    head: Option<HeadProjection>,
}

impl PullRequestProjection {
    fn head_sha(&self) -> Option<CommitSha> {
        self.head
            .as_ref()
            .and_then(|head| head.sha.as_ref())
            .and_then(Value::as_str)
            .and_then(|sha| CommitSha::parse(sha).ok())
    }
}

/// Authenticated, bounded dependencies for one pull-request queue event.
pub(super) struct QueueProcessor<'a> {
    pub(super) repository_id: RepositoryId,
    pub(super) repository_name: &'a CanonicalRepositoryName,
    pub(super) delivery_id: &'a DeliveryId,
    pub(super) event_type: EventType,
    pub(super) action: Action,
    pub(super) received_at: OffsetDateTime,
    pub(super) store: &'a MergeQueueStore,
    pub(super) metrics: &'a Metrics,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PullRequestTransition {
    Enqueue,
    Complete { completion: MergeQueueCompletion },
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
        let Some(transition) = self.transition(projection) else {
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

        let update_span = trace::operation_span(Operation::MergeQueueUpdate);
        trace::set_queue_entity(&update_span, QueueEntity::PullRequest);
        trace::set_webhook_event(&update_span, self.event_type, self.action);
        trace::set_repository_name(&update_span, self.repository_name);
        trace::set_repository_id(&update_span, self.repository_id);
        trace::set_delivery_id(&update_span, self.delivery_id);
        trace::set_pull_request_number(&update_span, pull_request_number);
        if let Some(head_sha) = projection.head_sha() {
            trace::set_commit_sha(&update_span, &head_sha);
        }
        if let PullRequestTransition::Complete { completion } = transition {
            trace::set_merge_queue_completion(&update_span, completion);
        }

        let result = self
            .apply_transition(transition, pull_request_number, event_timestamp)
            .instrument(update_span.clone())
            .await;
        match result {
            Ok(outcome) => {
                trace::set_status(&update_span, outcome);
                Ok(())
            }
            Err(error) => {
                trace::add_failure_event(&update_span, OperationFailureReason::QueueState);
                trace::set_status(&update_span, OperationOutcome::Failure);
                Err(error)
            }
        }
    }

    fn transition(&self, projection: &PullRequestProjection) -> Option<PullRequestTransition> {
        match self.action {
            Action::Enqueued => Some(PullRequestTransition::Enqueue),
            Action::Dequeued => Some(PullRequestTransition::Complete {
                completion: MergeQueueCompletion::UnclassifiedDequeue,
            }),
            Action::Closed if projection.merged == Some(true) => {
                Some(PullRequestTransition::Complete {
                    completion: MergeQueueCompletion::PullRequestMerged,
                })
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
            | Action::Other => None,
        }
    }

    async fn apply_transition(
        &self,
        transition: PullRequestTransition,
        pull_request_number: PullRequestNumber,
        event_timestamp: QueueTimestamp,
    ) -> Result<OperationOutcome, MergeQueueStoreError> {
        match transition {
            PullRequestTransition::Enqueue => self
                .store
                .enqueue(self.repository_id, pull_request_number, &event_timestamp)
                .await
                .map(|transition| match transition {
                    EnqueueTransition::Created => OperationOutcome::Success,
                    EnqueueTransition::AlreadyActive => OperationOutcome::NoOp,
                }),
            PullRequestTransition::Complete { completion } => {
                let queue_completion = match completion {
                    MergeQueueCompletion::PullRequestMerged => {
                        QueueCompletion::pull_request_merged(event_timestamp)
                    }
                    MergeQueueCompletion::UnclassifiedDequeue => {
                        QueueCompletion::unclassified_dequeue(event_timestamp)
                    }
                };
                self.complete_attempt(pull_request_number, queue_completion, completion)
                    .await
            }
        }
    }

    async fn complete_attempt(
        &self,
        pull_request_number: PullRequestNumber,
        completion: QueueCompletion,
        metric_completion: MergeQueueCompletion,
    ) -> Result<OperationOutcome, MergeQueueStoreError> {
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
                Ok(OperationOutcome::Success)
            }
            CompletionTransition::AlreadyCompleted => Ok(OperationOutcome::NoOp),
            CompletionTransition::MissingActiveAttempt => {
                self.metrics.record_merge_queue_transition_failure(
                    QueueTransitionFailureReason::MissingActiveAttempt,
                );
                Ok(OperationOutcome::NoOp)
            }
        }
    }
}
