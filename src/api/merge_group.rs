use serde::Deserialize;
use serde_json::Value;

use crate::{
    api::pull_request::PullRequestProjection,
    metrics::{
        normalize_merge_group_destroyed_reason, Action, EventType, MergeGroupAction,
        MergeGroupReason,
    },
    telemetry::trace::CommitSha,
};

/// Minimal authenticated payload fields needed for generic action and merge-group processing.
#[derive(Deserialize)]
pub(super) struct EventProjection {
    action: Option<String>,
    reason: Option<Value>,
    merge_group: Option<MergeGroupProjection>,
    pull_request: Option<PullRequestProjection>,
}

#[derive(Deserialize)]
struct MergeGroupProjection {
    head_sha: Option<String>,
}

/// A normalized authenticated merge-group transition ready for specialized telemetry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct MergeGroupTransition {
    pub(super) action: MergeGroupAction,
    pub(super) reason: MergeGroupReason,
    pub(super) head_sha: Option<CommitSha>,
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

    /// Returns a supported merge-group transition using only bounded semantic values.
    pub(super) fn merge_group_transition(
        &self,
        event_type: EventType,
        action: Action,
    ) -> Option<MergeGroupTransition> {
        if event_type != EventType::MergeGroup {
            return None;
        }

        let (action, reason) = match action {
            Action::ChecksRequested => (MergeGroupAction::ChecksRequested, MergeGroupReason::None),
            Action::Destroyed => {
                // Missing, non-string, and unknown reasons intentionally share `other`; retaining
                // their raw distinctions would weaken the bounded telemetry vocabulary.
                let reason = self.reason.as_ref().and_then(Value::as_str).map_or(
                    MergeGroupReason::Other,
                    normalize_merge_group_destroyed_reason,
                );
                (MergeGroupAction::Destroyed, reason)
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
            | Action::Other => return None,
        };
        let head_sha = self
            .merge_group
            .as_ref()
            .and_then(|group| group.head_sha.as_deref())
            .and_then(|sha| CommitSha::parse(sha).ok());
        Some(MergeGroupTransition {
            action,
            reason,
            head_sha,
        })
    }
}
