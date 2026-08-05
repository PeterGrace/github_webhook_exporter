use serde::Deserialize;
use serde_json::Value;

use crate::{
    api::pull_request::PullRequestProjection,
    metrics::{
        normalize_merge_group_destroyed_reason, Action, EventType, MergeGroupAction,
        MergeGroupReason, Metrics,
    },
};

/// Minimal authenticated payload fields needed for generic action and merge-group processing.
#[derive(Deserialize)]
pub(super) struct EventProjection {
    action: Option<String>,
    reason: Option<Value>,
    pull_request: Option<PullRequestProjection>,
}

impl EventProjection {
    /// Returns the optional raw action for bounded generic normalization.
    pub(super) fn action(&self) -> Option<&str> {
        self.action.as_deref()
    }

    /// Returns the optional pull-request projection used for specialized queue processing.
    pub(super) fn pull_request(&self) -> Option<&PullRequestProjection> {
        self.pull_request.as_ref()
    }

    /// Records a supported specialized merge-group event using only bounded metric labels.
    pub(super) fn process_merge_group(
        &self,
        event_type: EventType,
        action: Action,
        metrics: &Metrics,
    ) {
        if event_type != EventType::MergeGroup {
            return;
        }

        match action {
            Action::ChecksRequested => metrics.record_merge_group_event(
                MergeGroupAction::ChecksRequested,
                MergeGroupReason::None,
            ),
            Action::Destroyed => {
                // Missing, non-string, and unknown reasons intentionally share `other`; retaining
                // their raw distinctions would weaken the bounded metric vocabulary.
                let reason = self.reason.as_ref().and_then(Value::as_str).map_or(
                    MergeGroupReason::Other,
                    normalize_merge_group_destroyed_reason,
                );
                metrics.record_merge_group_event(MergeGroupAction::Destroyed, reason);
            }
            Action::Assigned
            | Action::Closed
            | Action::Completed
            | Action::Created
            | Action::Deleted
            | Action::Dequeued
            | Action::Edited
            | Action::Enqueued
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
            | Action::Other => {
                // Unsupported merge-group actions remain visible only through generic metrics.
            }
        }
    }
}
