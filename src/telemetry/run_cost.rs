//! Per-run cost totals: one `ci.run.cost` span and one `ci run cost finalized` log per finalized
//! run attempt, both placed in the run's derived pipeline trace.

use std::{cmp::Ordering, collections::BTreeMap, ops::Add, time::SystemTime};

use opentelemetry::{
    trace::{Span as _, SpanKind, Tracer},
    KeyValue,
};
use tracing::{info, warn};

use crate::{
    domain::merge_queue::PullRequestNumber,
    security::CanonicalRepositoryName,
    telemetry::{
        pipeline::run_trace_context,
        trace::{
            attributed_pull_request_attribute, github_workflow_attribute,
            pipeline_result_attribute, price_table_version_attribute,
            pull_request_numbers_attribute, pull_request_resolution_attribute,
            repository_name_attribute, run_cost_source_attribute, run_cost_total_attribute,
            run_minutes_attribute, sentry_description_attribute, sentry_operation_attribute,
            usage_entries_attribute, workflow_event_attribute, workflow_file_attribute,
            workflow_run_attempt_attribute, CommitSha, CI_RUN_COST_OPERATION,
        },
        workflow::{
            append_pipeline_and_repository_context, DisplayName, WorkflowConclusion, WorkflowEvent,
            WorkflowFile, WorkflowRunAttempt, WorkflowRunId, WorkflowTraceEmitter,
            UNKNOWN_WORKFLOW_NAME,
        },
    },
};

const RUN_COST_SPAN_NAME: &str = "ci.run.cost";
/// The most `ci.cost.<source>_usd` or `ci.minutes.<label>` keys one run total carries.
pub(crate) const MAX_BREAKDOWN_KEYS: usize = 16;
/// The key that absorbs breakdown entries past [`MAX_BREAKDOWN_KEYS`].
pub(crate) const OTHER_BREAKDOWN_KEY: &str = "other";
/// The `ci.minutes.<label>` key of jobs billed at the default rate.
pub(crate) const DEFAULT_RUNNER_LABEL_KEY: &str = "default";
/// The `ci.minutes.<label>` key of jobs no price table priced.
pub(crate) const UNPRICED_RUNNER_LABEL_KEY: &str = "unpriced";
/// The `ci.minutes.<label>` key of priced jobs stored before runner labels were recorded.
pub(crate) const UNLABELLED_RUNNER_LABEL_KEY: &str = "unlabelled";
const MAX_BREAKDOWN_KEY_LENGTH: usize = 32;

/// Which webhook-state rule attributed a run to its pull request: the closed
/// `github.pr_resolution` vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PullRequestResolution {
    /// The run's `pull_requests` named exactly one pull request.
    PullRequests,
    /// A merge-queue run's `gh-readonly-queue/<base>/pr-<N>-<sha>` branch.
    MergeQueueBranch,
    /// `PR #<N>` in the run name of a comment- or `workflow_run`-triggered run.
    RunName,
    /// The run's head commit was seen on a `pull_request` delivery.
    CommitMap,
    /// No rule matched.
    None,
}

impl PullRequestResolution {
    const ALL: [Self; 5] = [
        Self::PullRequests,
        Self::MergeQueueBranch,
        Self::RunName,
        Self::CommitMap,
        Self::None,
    ];

    /// Returns the stored and exported value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::PullRequests => "pull_requests",
            Self::MergeQueueBranch => "merge_queue_branch",
            Self::RunName => "run_name",
            Self::CommitMap => "commit_map",
            Self::None => "none",
        }
    }

    /// Parses a value produced by [`Self::as_str`], rejecting anything else.
    pub(crate) fn from_stored(value: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|resolution| resolution.as_str() == value)
    }
}

/// Turns a runner label or usage source into an attribute-key fragment: lowercase ASCII letters
/// and digits are kept, everything else becomes `_`, and the result is cut to 32 characters.
pub(crate) fn breakdown_key(value: &str) -> Option<String> {
    let key = value
        .chars()
        .take(MAX_BREAKDOWN_KEY_LENGTH)
        .map(|character| {
            let character = character.to_ascii_lowercase();
            if character.is_ascii_lowercase() || character.is_ascii_digit() {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    (!key.is_empty()).then_some(key)
}

/// Merges equal keys and caps a breakdown at [`MAX_BREAKDOWN_KEYS`], folding the smallest
/// entries into [`OTHER_BREAKDOWN_KEY`], so the attribute key set of a run stays bounded.
///
/// The result is sorted by key.
pub(crate) fn bounded_breakdown<T>(
    entries: impl IntoIterator<Item = (String, T)>,
) -> Vec<(String, T)>
where
    T: Copy + PartialOrd + Add<Output = T>,
{
    let mut merged = BTreeMap::<String, T>::new();
    for (key, value) in entries {
        merged
            .entry(key)
            .and_modify(|total| *total = *total + value)
            .or_insert(value);
    }
    if merged.len() <= MAX_BREAKDOWN_KEYS {
        return merged.into_iter().collect();
    }

    let mut ranked = merged.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    let folded = ranked
        .split_off(MAX_BREAKDOWN_KEYS - 1)
        .into_iter()
        .map(|(_, value)| value)
        .reduce(|left, right| left + right);
    if let Some(folded) = folded {
        match ranked
            .iter_mut()
            .find(|(key, _)| key == OTHER_BREAKDOWN_KEY)
        {
            Some((_, other)) => *other = *other + folded,
            None => ranked.push((OTHER_BREAKDOWN_KEY.to_owned(), folded)),
        }
    }
    ranked.sort_by(|left, right| left.0.cmp(&right.0));
    ranked
}

/// One finalized run attempt's cost total and the attribution it was emitted with.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct RunCostTotal {
    pub(crate) repository_name: CanonicalRepositoryName,
    pub(crate) run_id: WorkflowRunId,
    pub(crate) run_attempt: WorkflowRunAttempt,
    pub(crate) workflow_name: Option<DisplayName>,
    pub(crate) workflow_file: Option<WorkflowFile>,
    pub(crate) event: WorkflowEvent,
    pub(crate) conclusion: WorkflowConclusion,
    pub(crate) head_sha: Option<CommitSha>,
    pub(crate) pull_request: Option<PullRequestNumber>,
    pub(crate) resolution: PullRequestResolution,
    pub(crate) total_usd: f64,
    /// Bounded `(source key, USD)` pairs, sorted by key.
    pub(crate) cost_by_source: Vec<(String, f64)>,
    /// Bounded `(runner label key, minutes)` pairs, sorted by key.
    pub(crate) minutes_by_label: Vec<(String, i64)>,
    pub(crate) usage_entries: i64,
    pub(crate) price_table_version: Option<String>,
    pub(crate) finalized_at: SystemTime,
}

impl RunCostTotal {
    fn workflow_name(&self) -> &str {
        self.workflow_name
            .as_ref()
            .map_or(UNKNOWN_WORKFLOW_NAME, DisplayName::as_str)
    }

    fn pull_requests(&self) -> &[PullRequestNumber] {
        self.pull_request.as_slice()
    }
}

impl WorkflowTraceEmitter {
    /// Emits one instantaneous `ci.run.cost` span, a root within the run's pipeline trace.
    pub(crate) fn emit_run_cost(&self, total: &RunCostTotal) {
        let Some(tracer) = self.tracer() else {
            return;
        };
        let mut span = tracer.build_with_context(
            tracer
                .span_builder(RUN_COST_SPAN_NAME)
                .with_kind(SpanKind::Internal)
                .with_start_time(total.finalized_at)
                .with_attributes(run_cost_attributes(total)),
            &run_trace_context(&total.repository_name, total.run_id, total.run_attempt),
        );
        span.end_with_timestamp(total.finalized_at);
    }
}

fn run_cost_attributes(total: &RunCostTotal) -> Vec<KeyValue> {
    let mut attributes =
        Vec::with_capacity(24 + total.cost_by_source.len() + total.minutes_by_label.len());
    append_pipeline_and_repository_context(
        &mut attributes,
        total.workflow_name(),
        &total.repository_name,
        total.run_id,
        total.head_sha.as_ref(),
    );
    attributes.push(repository_name_attribute(&total.repository_name));
    attributes.push(workflow_run_attempt_attribute(total.run_attempt));
    attributes.push(workflow_event_attribute(total.event));
    attributes.push(github_workflow_attribute(total.workflow_name()));
    if let Some(file) = &total.workflow_file {
        attributes.push(workflow_file_attribute(file));
    }
    attributes.push(pipeline_result_attribute(total.conclusion));
    if let Some(pull_requests) = pull_request_numbers_attribute(total.pull_requests()) {
        attributes.push(pull_requests);
    }
    if let Some(attributed) = attributed_pull_request_attribute(total.pull_requests()) {
        attributes.push(attributed);
    }
    attributes.push(pull_request_resolution_attribute(total.resolution.as_str()));
    attributes.push(run_cost_total_attribute(total.total_usd));
    attributes.extend(
        total
            .cost_by_source
            .iter()
            .map(|(source, cost_usd)| run_cost_source_attribute(source, *cost_usd)),
    );
    attributes.extend(
        total
            .minutes_by_label
            .iter()
            .map(|(label, minutes)| run_minutes_attribute(label, *minutes)),
    );
    attributes.push(usage_entries_attribute(total.usage_entries));
    if let Some(version) = &total.price_table_version {
        attributes.push(price_table_version_attribute(version));
    }
    attributes.push(sentry_operation_attribute(CI_RUN_COST_OPERATION));
    attributes.push(sentry_description_attribute(format!(
        "{} cost",
        total.workflow_name()
    )));
    attributes
}

fn encode_breakdown<T: std::fmt::Display>(breakdown: &[(String, T)]) -> String {
    breakdown
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join(",")
}

/// Logs `ci run cost finalized` with the span's attributes, plus `ci run unattributed` when no
/// pull request was resolved.
///
/// Log fields are fixed at compile time, so the per-source and per-label breakdowns that the span
/// carries as `ci.cost.<source>_usd` and `ci.minutes.<label>` are encoded here as
/// `key=value` lists in `ci.cost.by_source` and `ci.minutes.by_label`.
pub(crate) fn log_run_cost_finalized(total: &RunCostTotal) {
    let _run_trace_context =
        run_trace_context(&total.repository_name, total.run_id, total.run_attempt).attach();
    let cost_by_source = encode_breakdown(&total.cost_by_source);
    let minutes_by_label = encode_breakdown(&total.minutes_by_label);
    info!(
        parent: None,
        ci.cost.total_usd = total.total_usd,
        ci.cost.by_source = cost_by_source.as_str(),
        ci.minutes.by_label = minutes_by_label.as_str(),
        ci.usage.entries = total.usage_entries,
        ci.price_table.version = total.price_table_version.as_deref(),
        cicd.pipeline.result = total.conclusion.semantic_result(),
        github.workflow = total.workflow_name(),
        github.workflow_file = total.workflow_file.as_ref().map(WorkflowFile::as_str),
        github.workflow.event = total.event.as_str(),
        github.pull_request.number = total.pull_request.map(|number| number.get()),
        github.pr_resolution = total.resolution.as_str(),
        github.repository = total.repository_name.as_str(),
        vcs.ref.head.revision = total.head_sha.as_ref().map(CommitSha::as_str),
        cicd.pipeline.run.id = total.run_id.get(),
        github.workflow.run.attempt = total.run_attempt.get(),
        "ci run cost finalized"
    );
    if total.pull_request.is_none() {
        warn!(
            parent: None,
            ci.cost.total_usd = total.total_usd,
            github.workflow = total.workflow_name(),
            github.workflow.event = total.event.as_str(),
            github.repository = total.repository_name.as_str(),
            cicd.pipeline.run.id = total.run_id.get(),
            github.workflow.run.attempt = total.run_attempt.get(),
            "ci run unattributed"
        );
    }
}

/// Warns that one cost record arrived after its run attempt's total was final, so the total
/// leaves it out. It follows the record's own `ci cost recorded` log.
pub(crate) fn warn_cost_after_finalize(
    repository_name: &CanonicalRepositoryName,
    run_id: WorkflowRunId,
    run_attempt: WorkflowRunAttempt,
    source: &str,
    cost_usd: f64,
) {
    let _run_trace_context = run_trace_context(repository_name, run_id, run_attempt).attach();
    warn!(
        parent: None,
        ci.source = source,
        ci.cost_usd = cost_usd,
        github.repository = repository_name.as_str(),
        cicd.pipeline.run.id = run_id.get(),
        github.workflow.run.attempt = run_attempt.get(),
        "ci cost after finalize"
    );
}

#[cfg(test)]
mod tests {
    use super::{
        bounded_breakdown, breakdown_key, PullRequestResolution, MAX_BREAKDOWN_KEYS,
        OTHER_BREAKDOWN_KEY,
    };

    #[test]
    fn breakdown_keys_are_short_lowercase_attribute_fragments() {
        assert_eq!(breakdown_key("open-router").as_deref(), Some("open_router"));
        assert_eq!(
            breakdown_key("Ubuntu-24.04-XL").as_deref(),
            Some("ubuntu_24_04_xl")
        );
        assert_eq!(
            breakdown_key(&"x".repeat(40)).map(|key| key.len()),
            Some(32)
        );
        assert_eq!(breakdown_key(""), None);
    }

    #[test]
    fn breakdowns_merge_equal_keys_and_fold_the_smallest_past_the_cap() {
        assert_eq!(
            bounded_breakdown([
                ("a".to_owned(), 1_i64),
                ("a".to_owned(), 2),
                ("b".to_owned(), 3)
            ]),
            [("a".to_owned(), 3), ("b".to_owned(), 3)]
        );

        let many = (0..20_i64).map(|index| (format!("k{index:02}"), index));
        let bounded = bounded_breakdown(many);
        assert_eq!(bounded.len(), MAX_BREAKDOWN_KEYS);
        assert_eq!(
            bounded.iter().map(|(_, value)| value).sum::<i64>(),
            (0..20).sum::<i64>(),
            "folding keeps the sum"
        );
        let other = bounded
            .iter()
            .find(|(key, _)| key == OTHER_BREAKDOWN_KEY)
            .expect("smallest entries fold into other");
        assert_eq!(other.1, (0..5).sum::<i64>());
        assert!(!bounded.iter().any(|(key, _)| key == "k04"));
        assert!(bounded.iter().any(|(key, _)| key == "k05"));
    }

    #[test]
    fn resolutions_round_trip_through_storage() {
        for resolution in PullRequestResolution::ALL {
            assert_eq!(
                PullRequestResolution::from_stored(resolution.as_str()),
                Some(resolution)
            );
        }
        assert_eq!(PullRequestResolution::from_stored("guess"), None);
    }
}
