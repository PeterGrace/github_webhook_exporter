//! Per-commit cost totals: one parentless `ci.commit.cost` span and one `ci commit cost finalized`
//! log per finalized commit.

use std::time::SystemTime;

use opentelemetry::{
    trace::{Span as _, SpanKind, Tracer},
    Context, KeyValue,
};
use tracing::{info, warn};

use crate::{
    domain::merge_queue::PullRequestNumber,
    security::CanonicalRepositoryName,
    telemetry::{
        pr_cost::LateCost,
        trace::{
            attributed_pull_request_attribute, commit_cost_attributes,
            pull_request_number_attribute, repository_name_attribute, run_cost_source_attribute,
            run_cost_total_attribute, sentry_description_attribute, sentry_operation_attribute,
            workflow_head_revision_attribute, CommitCostSummary, CommitSha,
            CI_COMMIT_COST_OPERATION,
        },
        workflow::{WorkflowEvent, WorkflowTraceEmitter},
    },
};

const COMMIT_COST_SPAN_NAME: &str = "ci.commit.cost";
/// The abbreviated SHA length in the span description, matching `git log --oneline`.
const SHORT_SHA_LENGTH: usize = 7;

/// What caused a commit's runs: the closed `ci.commit.trigger` vocabulary.
///
/// One commit can be built under several triggers, such as a merge-queue commit that is later
/// pushed to the default branch. The record names the first of merge group, push and pull
/// request that any of its runs had.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum CommitTrigger {
    /// A merge-queue commit.
    MergeGroup,
    /// A commit pushed to a branch.
    Push,
    /// A pull request's head commit, or a commit with only vendor runs.
    PullRequest,
}

impl CommitTrigger {
    /// Returns the exported value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::MergeGroup => "merge_group",
            Self::Push => "push",
            Self::PullRequest => "pull_request",
        }
    }

    /// Returns the trigger a run of `event` counts under, or `None` when the event does not count
    /// toward its commit: comment, review, schedule, dispatch and chained runs run on a commit
    /// without being caused by it.
    pub(crate) const fn of_event(event: WorkflowEvent) -> Option<Self> {
        match event {
            WorkflowEvent::MergeGroup => Some(Self::MergeGroup),
            WorkflowEvent::Push => Some(Self::Push),
            WorkflowEvent::PullRequest | WorkflowEvent::PullRequestTarget => {
                Some(Self::PullRequest)
            }
            WorkflowEvent::PullRequestReview
            | WorkflowEvent::PullRequestReviewComment
            | WorkflowEvent::IssueComment
            | WorkflowEvent::Schedule
            | WorkflowEvent::WorkflowDispatch
            | WorkflowEvent::WorkflowRun
            | WorkflowEvent::Other => None,
        }
    }

    /// Returns the highest-priority trigger among `events`, defaulting to [`Self::PullRequest`]
    /// for a commit whose cost is only vendor runs, which are always pull-request reviews.
    pub(crate) fn of_events(events: impl IntoIterator<Item = WorkflowEvent>) -> Self {
        events
            .into_iter()
            .filter_map(Self::of_event)
            .min()
            .unwrap_or(Self::PullRequest)
    }
}

/// One finalized commit's cost record.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct CommitCostTotal {
    pub(crate) repository_name: CanonicalRepositoryName,
    pub(crate) commit_sha: CommitSha,
    pub(crate) trigger: CommitTrigger,
    /// The pull request most of its runs and vendor runs were attributed to, lowest number first
    /// on a tie; `None` when none were.
    pub(crate) pull_request: Option<PullRequestNumber>,
    /// `true` when the commit had runs finalized before commit totals began.
    pub(crate) partial: bool,
    /// Distinct workflow runs.
    pub(crate) runs: i64,
    /// Run attempts after each run's first.
    pub(crate) reruns: i64,
    pub(crate) vendor_runs: i64,
    pub(crate) total_usd: f64,
    /// Bounded `(source key, USD)` pairs, sorted by key.
    pub(crate) cost_by_source: Vec<(String, f64)>,
    pub(crate) finalized_at: SystemTime,
}

impl CommitCostTotal {
    fn summary(&self) -> CommitCostSummary {
        CommitCostSummary {
            trigger: self.trigger.as_str(),
            partial: self.partial,
            runs: self.runs,
            reruns: self.reruns,
            vendor_runs: self.vendor_runs,
        }
    }

    /// Returns the first seven characters of the commit SHA. A SHA is ASCII hexadecimal, so any
    /// byte offset is a character boundary.
    fn short_sha(&self) -> &str {
        &self.commit_sha.as_str()[..SHORT_SHA_LENGTH]
    }
}

impl WorkflowTraceEmitter {
    /// Emits one instantaneous, parentless `ci.commit.cost` span for `total`.
    pub(crate) fn emit_commit_cost(&self, total: &CommitCostTotal) {
        let Some(tracer) = self.tracer() else {
            return;
        };
        let mut span = tracer.build_with_context(
            tracer
                .span_builder(COMMIT_COST_SPAN_NAME)
                .with_kind(SpanKind::Internal)
                .with_start_time(total.finalized_at)
                .with_attributes(commit_cost_span_attributes(total)),
            &Context::new(),
        );
        span.end_with_timestamp(total.finalized_at);
    }
}

fn commit_cost_span_attributes(total: &CommitCostTotal) -> Vec<KeyValue> {
    let mut attributes = Vec::with_capacity(13 + total.cost_by_source.len());
    attributes.push(repository_name_attribute(&total.repository_name));
    attributes.push(workflow_head_revision_attribute(&total.commit_sha));
    if let Some(pull_request) = total.pull_request {
        attributes.push(pull_request_number_attribute(pull_request));
        attributes.extend(attributed_pull_request_attribute(std::slice::from_ref(
            &pull_request,
        )));
    }
    attributes.extend(commit_cost_attributes(&total.summary()));
    attributes.push(run_cost_total_attribute(total.total_usd));
    attributes.extend(
        total
            .cost_by_source
            .iter()
            .map(|(source, cost_usd)| run_cost_source_attribute(source, *cost_usd)),
    );
    attributes.push(sentry_operation_attribute(CI_COMMIT_COST_OPERATION));
    attributes.push(sentry_description_attribute(format!(
        "Commit {} cost",
        total.short_sha()
    )));
    attributes
}

/// Logs `ci commit cost finalized` with the span's attributes.
///
/// Like `ci pr cost finalized`, the per-source breakdown is a `key=value` list in
/// `ci.cost.by_source`, and the log is written even when the trace emitter is off.
pub(crate) fn log_commit_cost_finalized(total: &CommitCostTotal) {
    let _parentless_context = Context::new().attach();
    let cost_by_source = total
        .cost_by_source
        .iter()
        .map(|(source, cost_usd)| format!("{source}={cost_usd}"))
        .collect::<Vec<_>>()
        .join(",");
    let pull_request = total.pull_request.map(PullRequestNumber::get);
    info!(
        parent: None,
        vcs.ref.head.revision = total.commit_sha.as_str(),
        ci.pull_request.number = pull_request,
        ci.commit.trigger = total.trigger.as_str(),
        ci.commit.runs = total.runs,
        ci.commit.reruns = total.reruns,
        ci.commit.vendor_runs = total.vendor_runs,
        ci.commit.partial = total.partial,
        ci.cost.total_usd = total.total_usd,
        ci.cost.by_source = cost_by_source.as_str(),
        github.repository = total.repository_name.as_str(),
        "ci commit cost finalized"
    );
}

/// Warns that a cost reached a commit whose record was already final.
///
/// It is logged in the same context as `ci pr cost after finalize`.
pub(crate) fn warn_commit_cost_after_finalize(
    repository_name: &CanonicalRepositoryName,
    commit_sha: &CommitSha,
    late: LateCost<'_>,
) {
    let _trace_context = late.trace_context(repository_name).attach();
    let (cost_usd, run_id, run_attempt, vendor) = late.fields();
    warn!(
        parent: None,
        ci.cost_usd = cost_usd,
        ci.vendor = vendor,
        vcs.ref.head.revision = commit_sha.as_str(),
        github.repository = repository_name.as_str(),
        cicd.pipeline.run.id = run_id,
        github.workflow.run.attempt = run_attempt,
        "ci commit cost after finalize"
    );
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, time::SystemTime};

    use opentelemetry::Value;

    use super::{commit_cost_span_attributes, CommitCostTotal, CommitTrigger};
    use crate::{
        domain::merge_queue::PullRequestNumber,
        security::CanonicalRepositoryName,
        telemetry::{trace::CommitSha, workflow::WorkflowEvent},
    };

    fn total(pull_request: Option<i64>) -> CommitCostTotal {
        CommitCostTotal {
            repository_name: CanonicalRepositoryName::new("owner/repo").expect("name is valid"),
            commit_sha: CommitSha::parse("0123456789abcdef0123456789abcdef01234567")
                .expect("SHA parses"),
            trigger: CommitTrigger::Push,
            pull_request: pull_request
                .map(|number| PullRequestNumber::new(number).expect("PR is positive")),
            partial: false,
            runs: 3,
            reruns: 1,
            vendor_runs: 1,
            total_usd: 2.5,
            cost_by_source: vec![
                ("github_actions".to_owned(), 2.0),
                ("review_bot".to_owned(), 0.5),
            ],
            finalized_at: SystemTime::UNIX_EPOCH,
        }
    }

    fn keys(total: &CommitCostTotal) -> BTreeSet<String> {
        commit_cost_span_attributes(total)
            .iter()
            .map(|attribute| attribute.key.as_str().to_owned())
            .collect()
    }

    #[test]
    fn the_span_carries_the_record_and_one_key_per_source() {
        let total = total(Some(42));
        let attributes = commit_cost_span_attributes(&total);
        let keys = keys(&total);

        for key in [
            "github.repository.name",
            "vcs.ref.head.revision",
            "github.pull_request.number",
            "ci.pull_request.number",
            "ci.commit.trigger",
            "ci.commit.runs",
            "ci.commit.reruns",
            "ci.commit.vendor_runs",
            "ci.commit.partial",
            "ci.cost.total_usd",
            "ci.cost.github_actions_usd",
            "ci.cost.review_bot_usd",
            "sentry.op",
            "sentry.description",
        ] {
            assert!(keys.contains(key), "missing {key}");
        }
        assert_eq!(keys.len(), attributes.len(), "no attribute key repeats");
        let description = attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == "sentry.description")
            .map(|attribute| attribute.value.clone());
        assert_eq!(description, Some(Value::from("Commit 0123456 cost")));
    }

    #[test]
    fn a_commit_without_a_pull_request_leaves_both_numbers_off() {
        let keys = keys(&total(None));

        assert!(!keys.contains("github.pull_request.number"));
        assert!(!keys.contains("ci.pull_request.number"));
    }

    #[test]
    fn the_trigger_prefers_merge_group_then_push_then_pull_request() {
        use WorkflowEvent::{MergeGroup, PullRequest, PullRequestTarget, Push, Schedule};

        assert_eq!(
            CommitTrigger::of_events([PullRequest, Push, MergeGroup]),
            CommitTrigger::MergeGroup
        );
        assert_eq!(
            CommitTrigger::of_events([PullRequestTarget, Push]),
            CommitTrigger::Push
        );
        assert_eq!(
            CommitTrigger::of_events([PullRequestTarget]),
            CommitTrigger::PullRequest
        );
        assert_eq!(
            CommitTrigger::of_events([Schedule]),
            CommitTrigger::PullRequest
        );
        assert_eq!(CommitTrigger::of_events([]), CommitTrigger::PullRequest);
    }
}
