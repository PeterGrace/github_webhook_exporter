use serde::Deserialize;
use serde_json::Value;

use crate::{
    domain::merge_queue::PullRequestNumber,
    telemetry::{
        trace::CommitSha,
        workflow::{
            DisplayName, WorkflowBranch, WorkflowEvent, WorkflowFile, WorkflowPullRequests,
            WorkflowRunAttempt, WorkflowRunContext, WorkflowRunId,
        },
    },
};

#[derive(Deserialize)]
struct WorkflowRunEnvelope {
    workflow_run: WorkflowRunProjection,
}

#[derive(Deserialize)]
struct WorkflowRunProjection {
    id: i64,
    run_attempt: i64,
    name: Option<Value>,
    event: Option<Value>,
    head_branch: Option<Value>,
    head_sha: Option<Value>,
    path: Option<Value>,
    display_title: Option<Value>,
    #[serde(default)]
    pull_requests: Vec<PullRequestProjection>,
}

#[derive(Deserialize)]
struct PullRequestProjection {
    number: Option<i64>,
    head: Option<BranchProjection>,
    base: Option<BranchProjection>,
}

/// Bounded run-level metadata a pipeline-run summary trace needs beyond the correlated context.
///
/// This is a parts struct consumed directly by the pipeline emitter, so its already validated
/// fields are moved out rather than borrowed and cloned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct WorkflowRunSummary {
    pub(super) workflow_name: Option<DisplayName>,
    pub(super) head_sha: Option<CommitSha>,
    pub(super) pull_requests: WorkflowPullRequests,
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

    let pull_request = unique_pull_request(&workflow_run.pull_requests).or_else(|| {
        event
            .is_comment_triggered()
            .then(|| {
                workflow_run
                    .display_title
                    .as_ref()
                    .and_then(Value::as_str)
                    .and_then(pull_request_from_run_name)
            })
            .flatten()
    });
    let workflow_file = workflow_run
        .path
        .as_ref()
        .and_then(Value::as_str)
        .and_then(WorkflowFile::from_path);

    Some(
        WorkflowRunContext::new(run_id, run_attempt, event, source_branch, target_branch)
            .with_pull_request(pull_request)
            .with_workflow_file(workflow_file),
    )
}

/// Returns the run's pull request when its `pull_requests` name exactly one distinct number.
fn unique_pull_request(pull_requests: &[PullRequestProjection]) -> Option<PullRequestNumber> {
    let mut numbers = pull_requests
        .iter()
        .filter_map(|pull_request| pull_request.number)
        .filter_map(|number| PullRequestNumber::new(number).ok());
    let first = numbers.next()?;
    numbers.all(|number| number == first).then_some(first)
}

/// Reads the pull request from a run name that contains `PR #<number>`.
///
/// Comment-triggered workflows set `run-name` this way because GitHub leaves their
/// `pull_requests` empty. The marker must start the name or follow a non-alphanumeric character,
/// so `APR #3` does not match. The first match wins.
fn pull_request_from_run_name(run_name: &str) -> Option<PullRequestNumber> {
    const MARKER: &str = "PR #";
    let (start, _) = run_name.match_indices(MARKER).find(|(start, _)| {
        run_name[..*start]
            .chars()
            .next_back()
            .is_none_or(|previous| !previous.is_alphanumeric())
    })?;
    let rest = &run_name[start + MARKER.len()..];
    let digits_end = rest
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(rest.len());
    let number = rest[..digits_end].parse().ok()?;
    PullRequestNumber::new(number).ok()
}

/// Projects bounded run-level summary metadata from an authenticated workflow-run payload.
///
/// # Parameters
///
/// * `body` - The authenticated JSON request body.
///
/// # Returns
///
/// Sanitized workflow name, validated head revision, and at most the first 20 positive
/// pull-request numbers. Returns `None` only when the payload wrapper itself is malformed; every
/// individual field degrades to absent rather than failing the projection.
pub(super) fn project_run_summary(body: &[u8]) -> Option<WorkflowRunSummary> {
    let envelope: WorkflowRunEnvelope = serde_json::from_slice(body).ok()?;
    let workflow_run = envelope.workflow_run;

    Some(WorkflowRunSummary {
        workflow_name: workflow_run
            .name
            .as_ref()
            .and_then(Value::as_str)
            .and_then(DisplayName::sanitize),
        head_sha: workflow_run
            .head_sha
            .as_ref()
            .and_then(Value::as_str)
            .and_then(|value| CommitSha::parse(value).ok()),
        pull_requests: WorkflowPullRequests::new(
            workflow_run
                .pull_requests
                .iter()
                .filter_map(|pull_request| pull_request.number)
                .filter_map(|number| PullRequestNumber::new(number).ok()),
        ),
    })
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
    fn drops_dot_segment_branches_so_no_protection_lookup_can_use_them() {
        // Arrange: a signed payload whose branches would rewrite the GitHub API path.
        let context = project(json!({
            "workflow_run": {
                "id": 31,
                "run_attempt": 1,
                "event": "merge_group",
                "head_branch": "main/../../../../orgs/x",
                "pull_requests": [{
                    "head": {"ref": "feature/./x"},
                    "base": {"ref": "../main"}
                }]
            }
        }));

        // Assert: neither branch survives, so the required-check lookup has nothing to request.
        assert!(context.source_branch().is_none());
        assert!(context.target_branch().is_none());
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
    fn pull_request_of(workflow_run: serde_json::Value) -> Option<i64> {
        project(json!({ "workflow_run": workflow_run }))
            .pull_request()
            .map(|number| number.get())
    }

    #[test]
    fn a_run_is_attributed_to_its_single_pull_request() {
        assert_eq!(
            pull_request_of(json!({
                "id": 31, "run_attempt": 1, "event": "pull_request",
                "pull_requests": [{"number": 9201}, {"number": 9201}]
            })),
            Some(9201)
        );
        assert_eq!(
            pull_request_of(json!({
                "id": 31, "run_attempt": 1, "event": "pull_request",
                "pull_requests": [{"number": 9201}, {"number": 9202}]
            })),
            None
        );
    }

    #[test]
    fn comment_triggered_runs_read_their_pull_request_from_the_run_name() {
        for event in [
            "issue_comment",
            "pull_request_review",
            "pull_request_review_comment",
        ] {
            assert_eq!(
                pull_request_of(json!({
                    "id": 31, "run_attempt": 1, "event": event,
                    "display_title": "Claude on PR #9201 (comment)",
                    "pull_requests": []
                })),
                Some(9201),
                "{event}"
            );
        }
        assert_eq!(
            pull_request_of(json!({
                "id": 31, "run_attempt": 1, "event": "issue_comment",
                "display_title": "Claude on issue #77"
            })),
            None
        );
        assert_eq!(
            pull_request_of(json!({
                "id": 31, "run_attempt": 1, "event": "issue_comment",
                "display_title": "PR #0"
            })),
            None
        );
        for (title, expected) in [
            ("PR #12", Some(12)),
            ("Claude (PR #12)", Some(12)),
            ("APR #3", None),
            ("APR #3 then PR #4", Some(4)),
        ] {
            assert_eq!(
                pull_request_of(json!({
                    "id": 31, "run_attempt": 1, "event": "issue_comment",
                    "display_title": title
                })),
                expected,
                "{title}"
            );
        }
    }

    #[test]
    fn push_runs_never_read_a_pull_request_from_a_squash_merge_title() {
        assert_eq!(
            pull_request_of(json!({
                "id": 31, "run_attempt": 1, "event": "push",
                "display_title": "fix(api): handle errors PR #9088 (#9088)"
            })),
            None
        );
    }

    #[test]
    fn the_workflow_file_is_the_last_segment_of_the_workflow_path() {
        let context = project(json!({
            "workflow_run": {
                "id": 31, "run_attempt": 1, "event": "schedule",
                "path": ".github/workflows/ci.yaml"
            }
        }));

        assert_eq!(context.event(), WorkflowEvent::Schedule);
        assert_eq!(
            context.workflow_file().map(|file| file.as_str()),
            Some("ci.yaml")
        );
        let unusable = project(json!({
            "workflow_run": {
                "id": 31, "run_attempt": 1, "path": ".github/workflows/bad name.yaml"
            }
        }));
        assert!(unusable.workflow_file().is_none());
    }
}

#[cfg(test)]
mod run_summary_tests {
    use serde_json::json;

    use super::project_run_summary;
    use crate::telemetry::workflow::DisplayName;

    const HEAD_SHA: &str = "0123456789ABCDEF0123456789abcdef01234567";

    #[test]
    fn projects_sanitized_name_lowercase_revision_and_bounded_pull_requests() {
        let pull_requests = (1..=25)
            .map(|number| json!({"number": number}))
            .collect::<Vec<_>>();
        let body = serde_json::to_vec(&json!({
            "workflow_run": {
                "id": 31,
                "run_attempt": 1,
                "name": "Build\nWorkflow",
                "head_sha": HEAD_SHA,
                "pull_requests": pull_requests
            }
        }))
        .expect("fixture serializes");

        let summary = project_run_summary(&body).expect("summary projects");
        assert_eq!(
            summary.workflow_name.as_ref().map(DisplayName::as_str),
            Some("BuildWorkflow")
        );
        assert_eq!(
            summary.head_sha.as_ref().map(|sha| sha.as_str()),
            Some(&*HEAD_SHA.to_ascii_lowercase())
        );
        assert_eq!(summary.pull_requests.len(), 20);
        assert_eq!(summary.pull_requests.as_slice()[0].get(), 1);
    }

    #[test]
    fn malformed_fields_degrade_to_absent_without_failing_the_projection() {
        let body = serde_json::to_vec(&json!({
            "workflow_run": {
                "id": 31,
                "run_attempt": 1,
                "name": "\n\t",
                "head_sha": "not-a-sha",
                "pull_requests": [{"number": 0}, {"head": {"ref": "feature"}}]
            }
        }))
        .expect("fixture serializes");

        let summary = project_run_summary(&body).expect("summary projects");
        assert!(summary.workflow_name.is_none());
        assert!(summary.head_sha.is_none());
        assert!(summary.pull_requests.is_empty());
        assert!(project_run_summary(b"{").is_none());
    }
}
