//! Per-pull-request cost totals: one parentless `ci.pr.cost` span and one `ci pr cost finalized`
//! log per finalized pull request.

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
        pipeline::run_trace_context,
        trace::{
            attributed_pull_request_attribute, pull_request_cost_attributes,
            pull_request_number_attribute, repository_name_attribute, run_cost_source_attribute,
            run_cost_total_attribute, sentry_description_attribute, sentry_operation_attribute,
            PullRequestCostSummary, CI_PR_COST_OPERATION,
        },
        workflow::{WorkflowRunAttempt, WorkflowRunId, WorkflowTraceEmitter},
    },
};

const PR_COST_SPAN_NAME: &str = "ci.pr.cost";

/// How a pull request ended: the closed `github.pr_outcome` vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PullRequestOutcome {
    /// Still open, or reopened.
    Open,
    /// Closed by a merge.
    Merged,
    /// Closed without a merge.
    Closed,
}

impl PullRequestOutcome {
    const ALL: [Self; 3] = [Self::Open, Self::Merged, Self::Closed];

    /// Returns the stored and exported value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Merged => "merged",
            Self::Closed => "closed",
        }
    }

    /// Parses a value produced by [`Self::as_str`], rejecting anything else.
    pub(crate) fn from_stored(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|outcome| outcome.as_str() == value)
    }
}

/// Which rule finalized a pull request: the closed `ci.pr.finalized_by` vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PullRequestFinalizedBy {
    /// Its post-merge push runs on the merge commit had been quiet for the settle period.
    PostMergeRun,
    /// It was merged, but no post-merge run arrived within the post-merge timeout.
    Timeout,
    /// It was closed without merging and stayed closed for the grace period.
    ClosedGrace,
    /// It had no activity for the whole retention period.
    MaxAge,
}

impl PullRequestFinalizedBy {
    const ALL: [Self; 4] = [
        Self::PostMergeRun,
        Self::Timeout,
        Self::ClosedGrace,
        Self::MaxAge,
    ];

    /// Returns the stored and exported value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PostMergeRun => "post_merge_run",
            Self::Timeout => "timeout",
            Self::ClosedGrace => "closed_grace",
            Self::MaxAge => "max_age",
        }
    }

    /// Parses a value produced by [`Self::as_str`], rejecting anything else.
    pub(crate) fn from_stored(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|rule| rule.as_str() == value)
    }
}

/// One finalized pull request's cost record.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PullRequestCostTotal {
    pub(crate) repository_name: CanonicalRepositoryName,
    pub(crate) pull_request: PullRequestNumber,
    pub(crate) outcome: PullRequestOutcome,
    pub(crate) finalized_by: PullRequestFinalizedBy,
    /// `true` when the pull request may have runs from before run totals began.
    pub(crate) partial: bool,
    /// Distinct head commits across its runs and vendor runs.
    pub(crate) commits: i64,
    /// Distinct workflow runs.
    pub(crate) runs: i64,
    /// Run attempts after each run's first.
    pub(crate) reruns: i64,
    pub(crate) vendor_runs: i64,
    /// Hours from creation, or first activity when that is unknown, to close or finalization.
    pub(crate) open_hours: f64,
    pub(crate) total_usd: f64,
    /// Bounded `(source key, USD)` pairs, sorted by key.
    pub(crate) cost_by_source: Vec<(String, f64)>,
    pub(crate) finalized_at: SystemTime,
}

impl PullRequestCostTotal {
    fn summary(&self) -> PullRequestCostSummary {
        PullRequestCostSummary {
            outcome: self.outcome.as_str(),
            finalized_by: self.finalized_by.as_str(),
            partial: self.partial,
            commits: self.commits,
            runs: self.runs,
            reruns: self.reruns,
            vendor_runs: self.vendor_runs,
            open_hours: self.open_hours,
        }
    }
}

/// A cost that reached a pull request after its record was final, so the record leaves it out.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum LatePullRequestCost<'cost> {
    /// A finalized run attempt's total.
    Run {
        run_id: WorkflowRunId,
        run_attempt: WorkflowRunAttempt,
        total_usd: f64,
    },
    /// A priced vendor run.
    Vendor { vendor: &'cost str, cost_usd: f64 },
}

impl WorkflowTraceEmitter {
    /// Emits one instantaneous, parentless `ci.pr.cost` span for `total`.
    pub(crate) fn emit_pull_request_cost(&self, total: &PullRequestCostTotal) {
        let Some(tracer) = self.tracer() else {
            return;
        };
        let mut span = tracer.build_with_context(
            tracer
                .span_builder(PR_COST_SPAN_NAME)
                .with_kind(SpanKind::Internal)
                .with_start_time(total.finalized_at)
                .with_attributes(pull_request_cost_span_attributes(total)),
            &Context::new(),
        );
        span.end_with_timestamp(total.finalized_at);
    }
}

fn pull_request_cost_span_attributes(total: &PullRequestCostTotal) -> Vec<KeyValue> {
    let pull_requests = std::slice::from_ref(&total.pull_request);
    let mut attributes = Vec::with_capacity(14 + total.cost_by_source.len());
    attributes.push(repository_name_attribute(&total.repository_name));
    attributes.push(pull_request_number_attribute(total.pull_request));
    attributes.extend(attributed_pull_request_attribute(pull_requests));
    attributes.extend(pull_request_cost_attributes(&total.summary()));
    attributes.push(run_cost_total_attribute(total.total_usd));
    attributes.extend(
        total
            .cost_by_source
            .iter()
            .map(|(source, cost_usd)| run_cost_source_attribute(source, *cost_usd)),
    );
    attributes.push(sentry_operation_attribute(CI_PR_COST_OPERATION));
    attributes.push(sentry_description_attribute(format!(
        "PR #{} cost",
        total.pull_request.get()
    )));
    attributes
}

/// Logs `ci pr cost finalized` with the span's attributes.
///
/// Log fields are fixed at compile time, so the per-source breakdown the span carries as
/// `ci.cost.<source>_usd` is encoded here as a `key=value` list in `ci.cost.by_source`. The log is
/// independent of span export, so the record reaches the logs dataset even when the trace emitter
/// is off.
pub(crate) fn log_pull_request_cost_finalized(total: &PullRequestCostTotal) {
    let _parentless_context = Context::new().attach();
    let cost_by_source = total
        .cost_by_source
        .iter()
        .map(|(source, cost_usd)| format!("{source}={cost_usd}"))
        .collect::<Vec<_>>()
        .join(",");
    info!(
        parent: None,
        github.pull_request.number = total.pull_request.get(),
        ci.pull_request.number = total.pull_request.get(),
        github.pr_outcome = total.outcome.as_str(),
        ci.pr.commits = total.commits,
        ci.pr.runs = total.runs,
        ci.pr.reruns = total.reruns,
        ci.pr.vendor_runs = total.vendor_runs,
        ci.pr.open_hours = total.open_hours,
        ci.pr.finalized_by = total.finalized_by.as_str(),
        ci.pr.partial = total.partial,
        ci.cost.total_usd = total.total_usd,
        ci.cost.by_source = cost_by_source.as_str(),
        github.repository = total.repository_name.as_str(),
        "ci pr cost finalized"
    );
}

/// Warns that a cost reached a pull request whose record was already final.
///
/// A late run total is logged in its run's derived pipeline trace, like the run's other cost logs;
/// a vendor run has no workflow run, so its warning is parentless. The message differs from the
/// run-level `ci cost after finalize`, which means late to a run's total rather than to a pull
/// request's record.
pub(crate) fn warn_pull_request_cost_after_finalize(
    repository_name: &CanonicalRepositoryName,
    pull_request: PullRequestNumber,
    late: LatePullRequestCost<'_>,
) {
    let _trace_context = match late {
        LatePullRequestCost::Run {
            run_id,
            run_attempt,
            ..
        } => run_trace_context(repository_name, run_id, run_attempt),
        LatePullRequestCost::Vendor { .. } => Context::new(),
    }
    .attach();
    let (cost_usd, run_id, run_attempt, vendor) = match late {
        LatePullRequestCost::Run {
            run_id,
            run_attempt,
            total_usd,
        } => (total_usd, Some(run_id.get()), Some(run_attempt.get()), None),
        LatePullRequestCost::Vendor { vendor, cost_usd } => (cost_usd, None, None, Some(vendor)),
    };
    warn!(
        parent: None,
        ci.cost_usd = cost_usd,
        ci.vendor = vendor,
        github.pull_request.number = pull_request.get(),
        github.repository = repository_name.as_str(),
        cicd.pipeline.run.id = run_id,
        github.workflow.run.attempt = run_attempt,
        "ci pr cost after finalize"
    );
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, time::SystemTime};

    use super::{
        pull_request_cost_span_attributes, PullRequestCostTotal, PullRequestFinalizedBy,
        PullRequestOutcome,
    };
    use crate::{domain::merge_queue::PullRequestNumber, security::CanonicalRepositoryName};

    fn total() -> PullRequestCostTotal {
        PullRequestCostTotal {
            repository_name: CanonicalRepositoryName::new("owner/repo").expect("name is valid"),
            pull_request: PullRequestNumber::new(42).expect("PR is positive"),
            outcome: PullRequestOutcome::Merged,
            finalized_by: PullRequestFinalizedBy::PostMergeRun,
            partial: false,
            commits: 3,
            runs: 5,
            reruns: 1,
            vendor_runs: 2,
            open_hours: 26.5,
            total_usd: 4.0,
            cost_by_source: vec![
                ("anthropic".to_owned(), 1.0),
                ("github_actions".to_owned(), 2.0),
                ("review_bot".to_owned(), 1.0),
            ],
            finalized_at: SystemTime::UNIX_EPOCH,
        }
    }

    #[test]
    fn the_span_carries_the_record_and_one_key_per_source() {
        let attributes = pull_request_cost_span_attributes(&total());
        let keys = attributes
            .iter()
            .map(|attribute| attribute.key.as_str().to_owned())
            .collect::<BTreeSet<_>>();

        for key in [
            "github.repository.name",
            "github.pull_request.number",
            "ci.pull_request.number",
            "github.pr_outcome",
            "ci.pr.commits",
            "ci.pr.runs",
            "ci.pr.reruns",
            "ci.pr.vendor_runs",
            "ci.pr.open_hours",
            "ci.pr.finalized_by",
            "ci.pr.partial",
            "ci.cost.total_usd",
            "ci.cost.anthropic_usd",
            "ci.cost.github_actions_usd",
            "ci.cost.review_bot_usd",
            "sentry.op",
            "sentry.description",
        ] {
            assert!(keys.contains(key), "missing {key}");
        }
        assert_eq!(keys.len(), attributes.len(), "no attribute key repeats");
    }

    #[test]
    fn stored_vocabularies_round_trip_and_reject_unknown_values() {
        for outcome in PullRequestOutcome::ALL {
            assert_eq!(
                PullRequestOutcome::from_stored(outcome.as_str()),
                Some(outcome)
            );
        }
        for rule in PullRequestFinalizedBy::ALL {
            assert_eq!(
                PullRequestFinalizedBy::from_stored(rule.as_str()),
                Some(rule)
            );
        }
        assert_eq!(PullRequestOutcome::from_stored("draft"), None);
        assert_eq!(PullRequestFinalizedBy::from_stored("manual"), None);
    }
}
