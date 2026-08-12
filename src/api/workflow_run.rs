use serde::Deserialize;
use serde_json::Value;

use crate::telemetry::workflow::{
    WorkflowBranch, WorkflowEvent, WorkflowRunAttempt, WorkflowRunContext, WorkflowRunId,
};

#[derive(Deserialize)]
struct WorkflowRunEnvelope {
    workflow_run: WorkflowRunProjection,
}

#[derive(Deserialize)]
struct WorkflowRunProjection {
    id: i64,
    run_attempt: i64,
    event: Option<Value>,
    head_branch: Option<Value>,
    #[serde(default)]
    pull_requests: Vec<PullRequestProjection>,
}

#[derive(Deserialize)]
struct PullRequestProjection {
    head: Option<BranchProjection>,
    base: Option<BranchProjection>,
}

#[derive(Deserialize)]
struct BranchProjection {
    #[serde(rename = "ref")]
    reference: Option<Value>,
}

/// Projects bounded correlation metadata from an authenticated workflow-run payload.
pub(super) fn project_context(body: &[u8]) -> Option<WorkflowRunContext> {
    let envelope: WorkflowRunEnvelope = serde_json::from_slice(body).ok()?;
    let workflow_run = envelope.workflow_run;
    let run_id = WorkflowRunId::new(workflow_run.id).ok()?;
    let run_attempt = WorkflowRunAttempt::new(workflow_run.run_attempt).ok()?;
    let event = WorkflowEvent::normalize(workflow_run.event.as_ref().and_then(Value::as_str));
    let source_branch = workflow_run
        .head_branch
        .as_ref()
        .and_then(Value::as_str)
        .and_then(WorkflowBranch::sanitize)
        .or_else(|| {
            unique_branch(&workflow_run.pull_requests, |pull_request| {
                &pull_request.head
            })
        });
    let target_branch = unique_branch(&workflow_run.pull_requests, |pull_request| {
        &pull_request.base
    });

    Some(WorkflowRunContext::new(
        run_id,
        run_attempt,
        event,
        source_branch,
        target_branch,
    ))
}

fn unique_branch(
    pull_requests: &[PullRequestProjection],
    select: impl Fn(&PullRequestProjection) -> &Option<BranchProjection>,
) -> Option<WorkflowBranch> {
    let mut unique: Option<WorkflowBranch> = None;
    for branch in pull_requests
        .iter()
        .filter_map(|pull_request| select(pull_request).as_ref())
        .filter_map(|branch| branch.reference.as_ref())
        .filter_map(Value::as_str)
        .filter_map(WorkflowBranch::sanitize)
    {
        match unique.as_ref() {
            None => unique = Some(branch),
            Some(current) if current == &branch => {}
            Some(_) => return None,
        }
    }
    unique
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::project_context;
    use crate::telemetry::workflow::WorkflowEvent;

    fn project(value: serde_json::Value) -> super::WorkflowRunContext {
        let body = serde_json::to_vec(&value).expect("fixture serializes");
        project_context(&body).expect("workflow-run context projects")
    }

    #[test]
    fn projects_authoritative_event_and_workflow_branches() {
        let context = project(json!({
            "workflow_run": {
                "id": 31,
                "run_attempt": 2,
                "event": "merge_group",
                "head_branch": "gh-readonly-queue/main/pr-7-deadbeef",
                "pull_requests": [{
                    "head": {"ref": "feature/source"},
                    "base": {"ref": "main"}
                }]
            }
        }));

        assert_eq!(context.run_id().get(), 31);
        assert_eq!(context.run_attempt().get(), 2);
        assert_eq!(context.event(), WorkflowEvent::MergeGroup);
        assert_eq!(
            context.source_branch().map(|branch| branch.as_str()),
            Some("gh-readonly-queue/main/pr-7-deadbeef")
        );
        assert_eq!(
            context.target_branch().map(|branch| branch.as_str()),
            Some("main")
        );
    }

    #[test]
    fn falls_back_to_one_unique_pull_request_source_and_omits_ambiguous_target() {
        let context = project(json!({
            "workflow_run": {
                "id": 31,
                "run_attempt": 1,
                "event": "pull_request",
                "pull_requests": [
                    {"head": {"ref": "feature"}, "base": {"ref": "main"}},
                    {"head": {"ref": "feature"}, "base": {"ref": "release"}}
                ]
            }
        }));

        assert_eq!(context.event(), WorkflowEvent::PullRequest);
        assert_eq!(
            context.source_branch().map(|branch| branch.as_str()),
            Some("feature")
        );
        assert!(context.target_branch().is_none());
    }

    #[test]
    fn normalizes_unknown_events_and_sanitizes_bounded_branches() {
        let oversized = format!("feature\n{}", "x".repeat(300));
        let context = project(json!({
            "workflow_run": {
                "id": 31,
                "run_attempt": 1,
                "event": "untrusted-event",
                "head_branch": oversized,
                "pull_requests": []
            }
        }));

        assert_eq!(context.event(), WorkflowEvent::Other);
        let source = context.source_branch().expect("sanitized source exists");
        assert!(!source.as_str().contains('\n'));
        assert_eq!(source.as_str().chars().count(), 255);
    }

    #[test]
    fn rejects_non_positive_identifiers_and_omits_invalid_branch_values() {
        for (run_id, run_attempt) in [(0, 1), (31, 0)] {
            let body = serde_json::to_vec(&json!({
                "workflow_run": {
                    "id": run_id,
                    "run_attempt": run_attempt,
                    "event": "push"
                }
            }))
            .expect("fixture serializes");
            assert!(project_context(&body).is_none());
        }

        let context = project(json!({
            "workflow_run": {
                "id": 31,
                "run_attempt": 1,
                "event": null,
                "head_branch": 42,
                "pull_requests": [{"head": {"ref": "\n"}, "base": {"ref": null}}]
            }
        }));
        assert_eq!(context.event(), WorkflowEvent::Other);
        assert!(context.source_branch().is_none());
        assert!(context.target_branch().is_none());
    }
}
