//! Bounded workflow telemetry values used to project authenticated GitHub Actions history.

use std::{fmt, time::SystemTime};

use opentelemetry::{
    trace::{Span as _, Status, TraceContextExt, Tracer},
    Context, KeyValue,
};
use opentelemetry_sdk::trace::SdkTracer;
use thiserror::Error;

use crate::{
    domain::{delivery::DeliveryId, merge_queue::PullRequestNumber},
    security::CanonicalRepositoryName,
};

use super::trace::{
    commit_sha_attribute, delivery_id_attribute, pull_request_numbers_attribute,
    repository_name_attribute, timing_source_attribute, workflow_conclusion_attribute,
    workflow_job_id_attribute, workflow_name_attribute, workflow_pipeline_result_attribute,
    workflow_pipeline_run_id_attribute, workflow_pipeline_step_task_run_id_attribute,
    workflow_pipeline_task_run_id_attribute, workflow_pipeline_task_run_result_attribute,
    workflow_run_attempt_attribute, workflow_run_id_attribute, workflow_task_name_attribute,
    CommitSha,
};

const MAX_DISPLAY_NAME_LENGTH: usize = 128;
const MAX_PULL_REQUEST_COUNT: usize = 20;
const WORKFLOW_JOB_SPAN_NAME: &str = "github.workflow.job";
const WORKFLOW_STEP_SPAN_NAME: &str = "github.workflow.step";
const JOB_REQUIRED_ATTRIBUTE_COUNT: usize = 9;
const STEP_REQUIRED_ATTRIBUTE_COUNT: usize = 3;

/// A malformed workflow telemetry value.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("workflow telemetry value must be positive")]
pub(crate) struct WorkflowValueError;

/// A malformed workflow timing interval.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("workflow timing interval is invalid")]
pub(crate) struct WorkflowTimingError;

macro_rules! positive_i64_newtype {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub(crate) struct $name(i64);

        impl $name {
            /// Creates a validated positive workflow identifier.
            ///
            /// # Parameters
            ///
            /// * `value` - The candidate identifier.
            ///
            /// # Returns
            ///
            /// A bounded identifier when `value` is greater than zero.
            ///
            /// # Errors
            ///
            /// Returns [`WorkflowValueError`] when `value` is zero or negative.
            pub(crate) fn new(value: i64) -> Result<Self, WorkflowValueError> {
                if value > 0 {
                    Ok(Self(value))
                } else {
                    Err(WorkflowValueError)
                }
            }

            /// Returns the validated positive integer.
            pub(crate) const fn get(self) -> i64 {
                self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(concat!(stringify!($name), "([REDACTED])"))
            }
        }
    };
}

positive_i64_newtype!(
    WorkflowRunId,
    "A validated GitHub Actions workflow run identifier."
);
positive_i64_newtype!(
    WorkflowRunAttempt,
    "A validated GitHub Actions workflow run attempt."
);
positive_i64_newtype!(
    WorkflowJobId,
    "A validated GitHub Actions workflow job identifier."
);

/// A sanitized GitHub Actions display name.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DisplayName(String);

impl DisplayName {
    /// Removes Unicode control characters and retains at most the first 128 visible characters.
    ///
    /// # Parameters
    ///
    /// * `value` - The candidate workflow or step display name.
    ///
    /// # Returns
    ///
    /// A sanitized display name when at least one visible character remains, otherwise `None`.
    pub(crate) fn sanitize(value: &str) -> Option<Self> {
        let mut sanitized = String::with_capacity(value.len().min(MAX_DISPLAY_NAME_LENGTH));
        let mut retained = 0usize;

        for character in value.chars() {
            if character.is_control() {
                continue;
            }
            if retained == MAX_DISPLAY_NAME_LENGTH {
                break;
            }
            sanitized.push(character);
            retained += 1;
        }

        if sanitized.is_empty() {
            None
        } else {
            Some(Self(sanitized))
        }
    }

    /// Returns the sanitized display name.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl HistoricalTiming {
    /// Creates a reported historical interval when the bounds are ordered.
    ///
    /// # Parameters
    ///
    /// * `start` - The reported start time.
    /// * `end` - The reported end time.
    ///
    /// # Returns
    ///
    /// A reported historical interval.
    ///
    /// # Errors
    ///
    /// Returns [`WorkflowTimingError`] when `start` is after `end`.
    pub(crate) fn reported(
        start: SystemTime,
        end: SystemTime,
    ) -> Result<Self, WorkflowTimingError> {
        if start <= end {
            Ok(Self {
                start,
                end,
                source: TimingSource::Reported,
            })
        } else {
            Err(WorkflowTimingError)
        }
    }

    /// Creates a fallback historical interval anchored to a single instant.
    pub(crate) fn fallback(instant: SystemTime) -> Self {
        Self {
            start: instant,
            end: instant,
            source: TimingSource::Fallback,
        }
    }

    /// Creates a reported child interval bounded by its parent interval.
    ///
    /// # Parameters
    ///
    /// * `start` - The reported child start time.
    /// * `end` - The reported child end time.
    /// * `parent` - The selected parent interval.
    ///
    /// # Returns
    ///
    /// A reported child interval that remains inside `parent`.
    ///
    /// # Errors
    ///
    /// Returns [`WorkflowTimingError`] when the child interval is not ordered or falls outside the
    /// parent interval.
    pub(crate) fn reported_within_parent(
        start: SystemTime,
        end: SystemTime,
        parent: &Self,
    ) -> Result<Self, WorkflowTimingError> {
        if start <= end && start >= parent.start && end <= parent.end {
            Ok(Self {
                start,
                end,
                source: TimingSource::Reported,
            })
        } else {
            Err(WorkflowTimingError)
        }
    }

    /// Returns the historical start time.
    pub(crate) fn start(&self) -> SystemTime {
        self.start
    }

    /// Returns the historical end time.
    pub(crate) fn end(&self) -> SystemTime {
        self.end
    }

    /// Returns the timing source.
    pub(crate) fn source(&self) -> TimingSource {
        self.source
    }
}

impl WorkflowJobTrace {
    /// Creates a bounded workflow job trace from validated parts.
    ///
    /// # Parameters
    ///
    /// * `parts` - The already validated workflow job components.
    pub(crate) fn new(parts: WorkflowJobTraceParts) -> Self {
        Self {
            repository_name: parts.repository_name,
            delivery_id: parts.delivery_id,
            workflow_name: parts.workflow_name,
            run_id: parts.run_id,
            run_attempt: parts.run_attempt,
            job_id: parts.job_id,
            job_name: parts.job_name,
            conclusion: parts.conclusion,
            head_sha: parts.head_sha,
            pull_requests: parts.pull_requests,
            timing: parts.timing,
            steps: parts.steps,
        }
    }

    /// Returns the canonical repository name.
    pub(crate) fn repository_name(&self) -> &CanonicalRepositoryName {
        &self.repository_name
    }

    /// Returns the delivery identifier.
    pub(crate) fn delivery_id(&self) -> DeliveryId {
        self.delivery_id
    }

    /// Returns the optional sanitized workflow name.
    pub(crate) fn workflow_name(&self) -> Option<&DisplayName> {
        self.workflow_name.as_ref()
    }

    /// Returns the workflow run identifier.
    pub(crate) fn run_id(&self) -> WorkflowRunId {
        self.run_id
    }

    /// Returns the workflow run attempt.
    pub(crate) fn run_attempt(&self) -> WorkflowRunAttempt {
        self.run_attempt
    }

    /// Returns the workflow job identifier.
    pub(crate) fn job_id(&self) -> WorkflowJobId {
        self.job_id
    }

    /// Returns the optional sanitized job name.
    pub(crate) fn job_name(&self) -> Option<&DisplayName> {
        self.job_name.as_ref()
    }

    /// Returns the bounded job conclusion.
    pub(crate) fn conclusion(&self) -> WorkflowConclusion {
        self.conclusion
    }

    /// Returns the optional validated commit SHA.
    pub(crate) fn head_sha(&self) -> Option<&CommitSha> {
        self.head_sha.as_ref()
    }

    /// Returns the bounded pull-request collection.
    pub(crate) fn pull_requests(&self) -> &[PullRequestNumber] {
        self.pull_requests.as_slice()
    }

    /// Returns the selected historical interval.
    pub(crate) fn timing(&self) -> &HistoricalTiming {
        &self.timing
    }

    /// Returns the validated workflow steps.
    pub(crate) fn steps(&self) -> &[WorkflowStepTrace] {
        &self.steps
    }
}

impl WorkflowStepTrace {
    /// Returns the validated step number.
    pub(crate) fn number(&self) -> i64 {
        self.number
    }

    /// Returns the optional sanitized step name.
    pub(crate) fn name(&self) -> Option<&DisplayName> {
        self.name.as_ref()
    }

    /// Returns the bounded step conclusion.
    pub(crate) fn conclusion(&self) -> WorkflowConclusion {
        self.conclusion
    }

    /// Returns the selected historical interval.
    pub(crate) fn timing(&self) -> &HistoricalTiming {
        &self.timing
    }
}

impl fmt::Debug for DisplayName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DisplayName([REDACTED])")
    }
}

/// Emits explicit-time historical workflow traces through a configured SDK tracer.
#[derive(Clone, Debug, Default)]
pub struct WorkflowTraceEmitter {
    tracer: Option<SdkTracer>,
}

impl WorkflowTraceEmitter {
    /// Creates a disabled historical workflow trace emitter.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Creates an enabled historical workflow trace emitter.
    ///
    /// # Parameters
    ///
    /// * `tracer` - The SDK tracer that owns the bounded span processor.
    pub(crate) fn new(tracer: SdkTracer) -> Self {
        Self {
            tracer: Some(tracer),
        }
    }

    /// Emits one independent historical workflow job span and its child step spans.
    ///
    /// # Parameters
    ///
    /// * `job` - The bounded historical workflow job trace to export.
    pub(crate) fn emit(&self, job: &WorkflowJobTrace) {
        let Some(tracer) = &self.tracer else {
            return;
        };

        let mut root = tracer.build_with_context(
            tracer
                .span_builder(WORKFLOW_JOB_SPAN_NAME)
                .with_start_time(job.timing().start())
                .with_attributes(job_attributes(job)),
            &Context::new(),
        );
        root.set_status(job.conclusion().status());
        let parent_context = Context::current_with_span(root);

        for step in job.steps() {
            let mut span = tracer.build_with_context(
                tracer
                    .span_builder(WORKFLOW_STEP_SPAN_NAME)
                    .with_start_time(step.timing().start())
                    .with_attributes(step_attributes(job.job_id(), step)),
                &parent_context,
            );
            span.set_status(step.conclusion().status());
            span.end_with_timestamp(step.timing().end());
        }

        parent_context.span().end_with_timestamp(job.timing().end());
    }
}

fn job_attributes(job: &WorkflowJobTrace) -> Vec<KeyValue> {
    let mut attributes = Vec::with_capacity(
        JOB_REQUIRED_ATTRIBUTE_COUNT
            + usize::from(job.workflow_name().is_some())
            + usize::from(job.job_name().is_some())
            + usize::from(job.conclusion().semantic_result().is_some())
            + usize::from(job.head_sha().is_some())
            + usize::from(!job.pull_requests().is_empty()),
    );
    attributes.push(repository_name_attribute(job.repository_name()));
    attributes.push(delivery_id_attribute(&job.delivery_id()));
    if let Some(workflow_name) = job.workflow_name() {
        attributes.push(workflow_name_attribute(workflow_name));
    }
    attributes.push(workflow_pipeline_run_id_attribute(job.run_id()));
    attributes.push(workflow_run_id_attribute(job.run_id()));
    attributes.push(workflow_run_attempt_attribute(job.run_attempt()));
    if let Some(job_name) = job.job_name() {
        attributes.push(workflow_task_name_attribute(job_name));
    }
    attributes.push(workflow_pipeline_task_run_id_attribute(job.job_id()));
    attributes.push(workflow_job_id_attribute(job.job_id()));
    attributes.push(workflow_conclusion_attribute(job.conclusion()));
    if let Some(result) = workflow_pipeline_result_attribute(job.conclusion()) {
        attributes.push(result);
    }
    if let Some(head_sha) = job.head_sha() {
        attributes.push(commit_sha_attribute(head_sha));
    }
    if let Some(pull_request_numbers) = pull_request_numbers_attribute(job.pull_requests()) {
        attributes.push(pull_request_numbers);
    }
    attributes.push(timing_source_attribute(job.timing().source()));
    attributes
}

fn step_attributes(job_id: WorkflowJobId, step: &WorkflowStepTrace) -> Vec<KeyValue> {
    let mut attributes = Vec::with_capacity(
        STEP_REQUIRED_ATTRIBUTE_COUNT
            + usize::from(step.name().is_some())
            + usize::from(step.conclusion().semantic_result().is_some()),
    );
    if let Some(name) = step.name() {
        attributes.push(workflow_task_name_attribute(name));
    }
    attributes.push(workflow_pipeline_step_task_run_id_attribute(job_id, step));
    attributes.push(workflow_conclusion_attribute(step.conclusion()));
    if let Some(result) = workflow_pipeline_task_run_result_attribute(step.conclusion()) {
        attributes.push(result);
    }
    attributes.push(timing_source_attribute(step.timing().source()));
    attributes
}

/// A bounded GitHub Actions conclusion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkflowConclusion {
    /// The job or step succeeded.
    Success,
    /// The job or step failed.
    Failure,
    /// The job or step was cancelled.
    Cancelled,
    /// The job or step was skipped.
    Skipped,
    /// The job or step timed out.
    TimedOut,
    /// The job or step concluded neutrally.
    Neutral,
    /// Any conclusion outside the approved vocabulary.
    Other,
}

impl WorkflowConclusion {
    /// Normalizes a raw GitHub conclusion into the bounded workflow vocabulary.
    ///
    /// # Parameters
    ///
    /// * `value` - The raw conclusion text, or `None` when the input is missing.
    ///
    /// # Returns
    ///
    /// The bounded workflow conclusion.
    pub(crate) fn normalize(value: Option<&str>) -> Self {
        match value {
            Some("success") => Self::Success,
            Some("failure") => Self::Failure,
            Some("cancelled") => Self::Cancelled,
            Some("skipped") => Self::Skipped,
            Some("timed_out") => Self::TimedOut,
            Some("neutral") => Self::Neutral,
            _ => Self::Other,
        }
    }

    /// Returns the GitHub conclusion vocabulary value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
            Self::TimedOut => "timed_out",
            Self::Neutral => "neutral",
            Self::Other => "other",
        }
    }

    /// Returns the semantic-convention result when one exists.
    ///
    /// The result is omitted for neutral and unsupported conclusions.
    pub(crate) const fn semantic_result(self) -> Option<&'static str> {
        match self {
            Self::Success => Some("success"),
            Self::Failure => Some("failure"),
            Self::Cancelled => Some("cancellation"),
            Self::Skipped => Some("skip"),
            Self::TimedOut => Some("timeout"),
            Self::Neutral | Self::Other => None,
        }
    }

    /// Returns the bounded OpenTelemetry status for this conclusion.
    ///
    /// `failure` and `timed_out` are recorded as an error; `success` is recorded as ok; the
    /// remaining conclusions leave status unset.
    pub(crate) fn status(self) -> Status {
        match self {
            Self::Success => Status::Ok,
            Self::Failure | Self::TimedOut => Status::error("workflow_failed"),
            Self::Cancelled | Self::Skipped | Self::Neutral | Self::Other => Status::Unset,
        }
    }
}

/// The source used to select a historical timing interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimingSource {
    /// The timestamps were accepted from the GitHub payload.
    Reported,
    /// The timestamps were synthesized from a bounded fallback.
    Fallback,
}

impl TimingSource {
    /// Returns the normalized timing-source value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Reported => "reported",
            Self::Fallback => "fallback",
        }
    }
}

/// A bounded historical interval and the source that selected it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoricalTiming {
    start: SystemTime,
    end: SystemTime,
    source: TimingSource,
}

/// A bounded collection of workflow pull-request numbers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkflowPullRequests(Vec<PullRequestNumber>);

impl WorkflowPullRequests {
    /// Creates a bounded pull-request collection while preserving input order.
    ///
    /// Only the first 20 validated pull-request numbers are retained.
    pub(crate) fn new<I>(values: I) -> Self
    where
        I: IntoIterator<Item = PullRequestNumber>,
    {
        let mut pull_requests = Vec::with_capacity(MAX_PULL_REQUEST_COUNT);
        for number in values.into_iter().take(MAX_PULL_REQUEST_COUNT) {
            pull_requests.push(number);
        }
        Self(pull_requests)
    }

    /// Returns the retained pull-request numbers.
    pub(crate) fn as_slice(&self) -> &[PullRequestNumber] {
        &self.0
    }

    /// Returns the number of retained pull-request numbers.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether no pull-request numbers were retained.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// The validated parts needed to construct a workflow job trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkflowJobTraceParts {
    pub(crate) repository_name: CanonicalRepositoryName,
    pub(crate) delivery_id: DeliveryId,
    pub(crate) workflow_name: Option<DisplayName>,
    pub(crate) run_id: WorkflowRunId,
    pub(crate) run_attempt: WorkflowRunAttempt,
    pub(crate) job_id: WorkflowJobId,
    pub(crate) job_name: Option<DisplayName>,
    pub(crate) conclusion: WorkflowConclusion,
    pub(crate) head_sha: Option<CommitSha>,
    pub(crate) pull_requests: WorkflowPullRequests,
    pub(crate) timing: HistoricalTiming,
    pub(crate) steps: Vec<WorkflowStepTrace>,
}

/// An owned workflow-job trace accepted by the telemetry emitter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkflowJobTrace {
    repository_name: CanonicalRepositoryName,
    delivery_id: DeliveryId,
    workflow_name: Option<DisplayName>,
    run_id: WorkflowRunId,
    run_attempt: WorkflowRunAttempt,
    job_id: WorkflowJobId,
    job_name: Option<DisplayName>,
    conclusion: WorkflowConclusion,
    head_sha: Option<CommitSha>,
    pull_requests: WorkflowPullRequests,
    timing: HistoricalTiming,
    steps: Vec<WorkflowStepTrace>,
}

/// An owned workflow-step trace accepted by the telemetry emitter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkflowStepTrace {
    number: i64,
    name: Option<DisplayName>,
    conclusion: WorkflowConclusion,
    timing: HistoricalTiming,
}

impl WorkflowStepTrace {
    /// Creates a bounded workflow step trace.
    ///
    /// # Parameters
    ///
    /// * `number` - The positive step number.
    /// * `name` - The optional sanitized step name.
    /// * `conclusion` - The bounded step conclusion.
    /// * `timing` - The selected historical timing interval.
    ///
    /// # Returns
    ///
    /// A bounded step trace when `number` is positive.
    ///
    /// # Errors
    ///
    /// Returns [`WorkflowValueError`] when `number` is zero or negative.
    pub(crate) fn new(
        number: i64,
        name: Option<DisplayName>,
        conclusion: WorkflowConclusion,
        timing: HistoricalTiming,
    ) -> Result<Self, WorkflowValueError> {
        if number > 0 {
            Ok(Self {
                number,
                name,
                conclusion,
                timing,
            })
        } else {
            Err(WorkflowValueError)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeSet,
        sync::{Arc, Mutex},
        time::{Duration, SystemTime},
    };

    use opentelemetry::{
        trace::{SpanId, Status, TraceContextExt, Tracer, TracerProvider as _},
        Array, Context, Value,
    };
    use opentelemetry_sdk::{
        error::OTelSdkResult,
        trace::{SdkTracerProvider, SimpleSpanProcessor, SpanData, SpanExporter},
    };

    use super::{
        DisplayName, HistoricalTiming, TimingSource, WorkflowConclusion, WorkflowJobId,
        WorkflowJobTrace, WorkflowJobTraceParts, WorkflowPullRequests, WorkflowRunAttempt,
        WorkflowRunId, WorkflowStepTrace, WorkflowTraceEmitter,
    };
    use crate::{domain::merge_queue::PullRequestNumber, telemetry::trace::CommitSha};

    #[test]
    fn positive_workflow_identifiers_reject_zero_and_negative_values() {
        assert!(WorkflowRunId::new(1).is_ok());
        assert!(WorkflowRunAttempt::new(1).is_ok());
        assert!(WorkflowJobId::new(1).is_ok());
        assert!(WorkflowRunId::new(0).is_err());
        assert!(WorkflowRunAttempt::new(-1).is_err());
        assert!(WorkflowJobId::new(0).is_err());
    }

    #[test]
    fn display_names_remove_controls_and_stop_after_128_characters() {
        let input = format!("alpha\n{}omega", "x".repeat(200));
        let name = DisplayName::sanitize(&input).expect("visible characters remain");
        assert_eq!(name.as_str().chars().count(), 128);
        assert!(!name.as_str().chars().any(char::is_control));
        assert_eq!(DisplayName::sanitize("\n\r\t"), None);
    }

    #[test]
    fn conclusions_have_a_closed_normalized_vocabulary() {
        let cases = [
            (
                Some("success"),
                WorkflowConclusion::Success,
                Some("success"),
            ),
            (
                Some("failure"),
                WorkflowConclusion::Failure,
                Some("failure"),
            ),
            (
                Some("cancelled"),
                WorkflowConclusion::Cancelled,
                Some("cancellation"),
            ),
            (Some("skipped"), WorkflowConclusion::Skipped, Some("skip")),
            (
                Some("timed_out"),
                WorkflowConclusion::TimedOut,
                Some("timeout"),
            ),
            (Some("neutral"), WorkflowConclusion::Neutral, None),
            (Some("private-unknown"), WorkflowConclusion::Other, None),
            (None, WorkflowConclusion::Other, None),
        ];
        for (raw, expected, semantic_result) in cases {
            let conclusion = WorkflowConclusion::normalize(raw);
            assert_eq!(conclusion, expected);
            assert_eq!(conclusion.semantic_result(), semantic_result);
        }
    }

    #[test]
    fn workflow_conclusions_map_to_status_and_strings() {
        let cases = [
            (
                WorkflowConclusion::Success,
                "success",
                Some("success"),
                Some(Status::Ok),
            ),
            (
                WorkflowConclusion::Failure,
                "failure",
                Some("failure"),
                Some(Status::error("workflow_failed")),
            ),
            (
                WorkflowConclusion::Cancelled,
                "cancelled",
                Some("cancellation"),
                Some(Status::Unset),
            ),
            (
                WorkflowConclusion::Skipped,
                "skipped",
                Some("skip"),
                Some(Status::Unset),
            ),
            (
                WorkflowConclusion::TimedOut,
                "timed_out",
                Some("timeout"),
                Some(Status::error("workflow_failed")),
            ),
            (
                WorkflowConclusion::Neutral,
                "neutral",
                None,
                Some(Status::Unset),
            ),
            (
                WorkflowConclusion::Other,
                "other",
                None,
                Some(Status::Unset),
            ),
        ];

        for (conclusion, expected, semantic_result, expected_status) in cases {
            assert_eq!(conclusion.as_str(), expected);
            assert_eq!(conclusion.semantic_result(), semantic_result);
            assert_eq!(
                conclusion.status(),
                expected_status.expect("status is always available")
            );
        }
    }

    #[test]
    fn workflow_value_debug_output_is_redacted() {
        let display_name = DisplayName::sanitize("Build Workflow").expect("display name is valid");
        let run_id = WorkflowRunId::new(7).expect("run id is valid");
        let run_attempt = WorkflowRunAttempt::new(2).expect("run attempt is valid");
        let job_id = WorkflowJobId::new(9).expect("job id is valid");

        assert!(!format!("{display_name:?}").contains("Build Workflow"));
        assert!(!format!("{run_id:?}").contains('7'));
        assert!(!format!("{run_attempt:?}").contains('2'));
        assert!(!format!("{job_id:?}").contains('9'));
    }

    #[test]
    fn historical_timing_constructors_enforce_order_and_parent_bounds() {
        let parent_start = SystemTime::UNIX_EPOCH + Duration::from_secs(10);
        let parent_end = SystemTime::UNIX_EPOCH + Duration::from_secs(20);
        let parent = HistoricalTiming::reported(parent_start, parent_end)
            .expect("ordered reported interval is valid");

        assert_eq!(parent.start(), parent_start);
        assert_eq!(parent.end(), parent_end);
        assert_eq!(parent.source(), TimingSource::Reported);

        let fallback = HistoricalTiming::fallback(parent_end);
        assert_eq!(fallback.start(), parent_end);
        assert_eq!(fallback.end(), parent_end);
        assert_eq!(fallback.source(), TimingSource::Fallback);

        let child = HistoricalTiming::reported_within_parent(
            parent_start + Duration::from_secs(1),
            parent_end - Duration::from_secs(1),
            &parent,
        )
        .expect("child interval fits inside parent");
        assert_eq!(child.source(), TimingSource::Reported);

        assert!(HistoricalTiming::reported(parent_end, parent_start).is_err());
        assert!(HistoricalTiming::reported_within_parent(
            parent_start - Duration::from_secs(1),
            parent_end,
            &parent,
        )
        .is_err());
        assert!(HistoricalTiming::reported_within_parent(
            parent_start,
            parent_end + Duration::from_secs(1),
            &parent,
        )
        .is_err());
    }

    #[test]
    fn pull_request_collections_keep_input_order_and_cap_at_twenty() {
        let pull_requests = WorkflowPullRequests::new((1..=25).map(|number| {
            PullRequestNumber::new(number).expect("pull request number is positive")
        }));

        assert_eq!(pull_requests.len(), 20);
        assert!(!pull_requests.is_empty());
        assert_eq!(
            pull_requests
                .as_slice()
                .iter()
                .map(|number| number.get())
                .collect::<Vec<_>>(),
            (1..=20).collect::<Vec<_>>()
        );
    }

    #[test]
    fn workflow_job_trace_exposes_bounded_accessors() {
        let repository_name = crate::security::CanonicalRepositoryName::new("Owner/Repository")
            .expect("repository name is valid");
        let delivery_id =
            crate::domain::delivery::DeliveryId::parse("550e8400-e29b-41d4-a716-446655440000")
                .expect("delivery id is valid");
        let workflow_name = DisplayName::sanitize("Build\nWorkflow");
        let job_name = DisplayName::sanitize("Linux\tJob");
        let run_id = WorkflowRunId::new(31).expect("run id is valid");
        let run_attempt = WorkflowRunAttempt::new(2).expect("run attempt is valid");
        let job_id = WorkflowJobId::new(41).expect("job id is valid");
        let head_sha = super::CommitSha::parse("0123456789abcdef0123456789abcdef01234567")
            .expect("commit sha is valid");
        let timing = HistoricalTiming::reported(
            SystemTime::UNIX_EPOCH + Duration::from_secs(10),
            SystemTime::UNIX_EPOCH + Duration::from_secs(20),
        )
        .expect("job interval is valid");
        let step_timing = HistoricalTiming::reported_within_parent(
            SystemTime::UNIX_EPOCH + Duration::from_secs(12),
            SystemTime::UNIX_EPOCH + Duration::from_secs(18),
            &timing,
        )
        .expect("step interval fits inside job interval");
        let step = WorkflowStepTrace::new(1, None, WorkflowConclusion::Success, step_timing)
            .expect("step number is positive");
        let pull_requests = WorkflowPullRequests::new(vec![
            PullRequestNumber::new(7).expect("pull request number is positive")
        ]);
        let job = WorkflowJobTrace::new(WorkflowJobTraceParts {
            repository_name: repository_name.clone(),
            delivery_id,
            workflow_name: workflow_name.clone(),
            run_id,
            run_attempt,
            job_id,
            job_name: job_name.clone(),
            conclusion: WorkflowConclusion::Success,
            head_sha: Some(head_sha.clone()),
            pull_requests,
            timing: timing.clone(),
            steps: vec![step.clone()],
        });

        assert_eq!(job.repository_name(), &repository_name);
        assert_eq!(job.delivery_id(), delivery_id);
        assert_eq!(job.workflow_name(), workflow_name.as_ref());
        assert_eq!(job.run_id(), run_id);
        assert_eq!(job.run_attempt(), run_attempt);
        assert_eq!(job.job_id(), job_id);
        assert_eq!(job.job_name(), job_name.as_ref());
        assert_eq!(job.conclusion(), WorkflowConclusion::Success);
        assert_eq!(job.head_sha(), Some(&head_sha));
        assert_eq!(job.pull_requests().len(), 1);
        assert_eq!(job.timing(), &timing);
        assert_eq!(job.steps(), &[step]);
        assert_eq!(job.pull_requests()[0].get(), 7);
    }

    #[derive(Clone, Default, Debug)]
    struct CollectingSpanExporter(Arc<Mutex<Vec<SpanData>>>);

    impl CollectingSpanExporter {
        fn finished_spans(&self) -> Vec<SpanData> {
            self.0
                .lock()
                .expect("span capture lock is available")
                .clone()
        }
    }

    impl SpanExporter for CollectingSpanExporter {
        async fn export(&self, batch: Vec<SpanData>) -> OTelSdkResult {
            self.0
                .lock()
                .expect("span capture lock is available")
                .extend(batch);
            Ok(())
        }
    }

    #[test]
    fn workflow_step_trace_rejects_non_positive_numbers() {
        let timing = HistoricalTiming::reported(
            SystemTime::UNIX_EPOCH,
            SystemTime::UNIX_EPOCH + Duration::from_secs(1),
        )
        .expect("ordered interval is valid");

        assert!(
            WorkflowStepTrace::new(1, None, WorkflowConclusion::Success, timing.clone()).is_ok()
        );
        assert!(
            WorkflowStepTrace::new(0, None, WorkflowConclusion::Success, timing.clone()).is_err()
        );
        assert!(WorkflowStepTrace::new(-1, None, WorkflowConclusion::Success, timing).is_err());
    }

    #[test]
    fn emitter_exports_independent_historical_job_and_step_spans() {
        let exporter = CollectingSpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
            .build();
        let tracer = provider.tracer("github_webhook_exporter");
        let emitter = WorkflowTraceEmitter::new(tracer.clone());

        let job_start = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000);
        let first_step_start = job_start + Duration::from_secs(5);
        let first_step_end = first_step_start + Duration::from_secs(10);
        let second_step_start = first_step_end + Duration::from_secs(5);
        let second_step_end = second_step_start + Duration::from_secs(15);
        let job_end = second_step_end + Duration::from_secs(5);
        let job_timing =
            HistoricalTiming::reported(job_start, job_end).expect("job timing is ordered");
        let first_step_timing =
            HistoricalTiming::reported_within_parent(first_step_start, first_step_end, &job_timing)
                .expect("first step timing fits inside job timing");
        let second_step_timing = HistoricalTiming::reported_within_parent(
            second_step_start,
            second_step_end,
            &job_timing,
        )
        .expect("second step timing fits inside job timing");

        let repository_name = crate::security::CanonicalRepositoryName::new("Owner/Repository")
            .expect("repository name is valid");
        let delivery_id =
            crate::domain::delivery::DeliveryId::parse("550e8400-e29b-41d4-a716-446655440000")
                .expect("delivery id is valid");
        let run_id = WorkflowRunId::new(31).expect("run id is valid");
        let run_attempt = WorkflowRunAttempt::new(2).expect("run attempt is valid");
        let job_id = WorkflowJobId::new(41).expect("job id is valid");
        let head_sha = CommitSha::parse("0123456789abcdef0123456789abcdef01234567")
            .expect("commit sha is valid");
        let first_pull_request_number =
            PullRequestNumber::new(7).expect("first pull request number is positive");
        let second_pull_request_number =
            PullRequestNumber::new(11).expect("second pull request number is positive");
        let job = WorkflowJobTrace::new(WorkflowJobTraceParts {
            repository_name,
            delivery_id,
            workflow_name: DisplayName::sanitize("Build Workflow"),
            run_id,
            run_attempt,
            job_id,
            job_name: DisplayName::sanitize("Linux Job"),
            conclusion: WorkflowConclusion::Failure,
            head_sha: Some(head_sha),
            pull_requests: WorkflowPullRequests::new([
                first_pull_request_number,
                second_pull_request_number,
            ]),
            timing: job_timing.clone(),
            steps: vec![
                WorkflowStepTrace::new(
                    1,
                    DisplayName::sanitize("Checkout"),
                    WorkflowConclusion::Success,
                    first_step_timing.clone(),
                )
                .expect("first step is valid"),
                WorkflowStepTrace::new(
                    2,
                    DisplayName::sanitize("Test"),
                    WorkflowConclusion::TimedOut,
                    second_step_timing.clone(),
                )
                .expect("second step is valid"),
            ],
        });

        let active_span = tracer.span_builder("active.request").start(&tracer);
        let active_context = Context::current_with_span(active_span);
        let active_guard = active_context.clone().attach();
        emitter.emit(&job);
        drop(active_guard);
        active_context.span().end();

        provider.force_flush().expect("spans flush");
        let spans = exporter.finished_spans();

        assert_eq!(
            spans
                .iter()
                .filter(|span| span.name == "github.workflow.job")
                .count(),
            1
        );
        assert_eq!(
            spans
                .iter()
                .filter(|span| span.name == "github.workflow.step")
                .count(),
            2
        );

        let job = spans
            .iter()
            .find(|span| span.name == "github.workflow.job")
            .expect("job span is exported");
        let mut steps = spans
            .iter()
            .filter(|span| span.name == "github.workflow.step")
            .collect::<Vec<_>>();
        steps.sort_by_key(|span| span.start_time);

        assert_eq!(job.parent_span_id, SpanId::INVALID);
        assert!(steps
            .iter()
            .all(|step| step.parent_span_id == job.span_context.span_id()));
        assert_eq!(job.start_time, job_timing.start());
        assert_eq!(job.end_time, job_timing.end());
        assert_eq!(steps[0].start_time, first_step_timing.start());
        assert_eq!(steps[0].end_time, first_step_timing.end());
        assert_eq!(steps[1].start_time, second_step_timing.start());
        assert_eq!(steps[1].end_time, second_step_timing.end());
        assert_eq!(job.status, WorkflowConclusion::Failure.status());
        assert_eq!(steps[0].status, WorkflowConclusion::Success.status());
        assert_eq!(steps[1].status, WorkflowConclusion::TimedOut.status());

        let expected_job_keys = BTreeSet::from([
            "cicd.pipeline.name",
            "cicd.pipeline.result",
            "cicd.pipeline.run.id",
            "cicd.pipeline.task.name",
            "cicd.pipeline.task.run.id",
            "github.commit.sha",
            "github.delivery.id",
            "github.pull_request.number",
            "github.repository.name",
            "github.workflow.conclusion",
            "github.workflow.job.id",
            "github.workflow.run.attempt",
            "github.workflow.run.id",
            "timing_source",
        ]);
        assert_eq!(attribute_keys(job), expected_job_keys);
        assert_string_attribute(job, "github.repository.name", "owner/repository");
        assert_string_attribute(
            job,
            "github.delivery.id",
            "550e8400-e29b-41d4-a716-446655440000",
        );
        assert_string_attribute(job, "cicd.pipeline.name", "Build Workflow");
        assert_string_attribute(job, "cicd.pipeline.run.id", "31");
        assert_string_attribute(job, "github.workflow.run.id", "31");
        assert_string_attribute(job, "github.workflow.run.attempt", "2");
        assert_string_attribute(job, "cicd.pipeline.task.name", "Linux Job");
        assert_string_attribute(job, "cicd.pipeline.task.run.id", "41");
        assert_string_attribute(job, "github.workflow.job.id", "41");
        assert_string_attribute(job, "github.workflow.conclusion", "failure");
        assert_string_attribute(job, "cicd.pipeline.result", "failure");
        assert_string_attribute(
            job,
            "github.commit.sha",
            "0123456789abcdef0123456789abcdef01234567",
        );
        assert_i64_array_attribute(job, "github.pull_request.number", &[7, 11]);
        assert_string_attribute(job, "timing_source", "reported");

        let expected_step_keys = BTreeSet::from([
            "cicd.pipeline.task.name",
            "cicd.pipeline.task.run.id",
            "cicd.pipeline.task.run.result",
            "github.workflow.conclusion",
            "timing_source",
        ]);
        assert_eq!(attribute_keys(steps[0]), expected_step_keys);
        assert_eq!(attribute_keys(steps[1]), expected_step_keys);
        assert_string_attribute(steps[0], "cicd.pipeline.task.name", "Checkout");
        assert_string_attribute(steps[0], "cicd.pipeline.task.run.id", "41:1");
        assert_string_attribute(steps[0], "github.workflow.conclusion", "success");
        assert_string_attribute(steps[0], "cicd.pipeline.task.run.result", "success");
        assert_string_attribute(steps[0], "timing_source", "reported");
        assert_string_attribute(steps[1], "cicd.pipeline.task.name", "Test");
        assert_string_attribute(steps[1], "cicd.pipeline.task.run.id", "41:2");
        assert_string_attribute(steps[1], "github.workflow.conclusion", "timed_out");
        assert_string_attribute(steps[1], "cicd.pipeline.task.run.result", "timeout");
        assert_string_attribute(steps[1], "timing_source", "reported");

        for step in steps {
            for duplicated_identifier in [
                "cicd.pipeline.name",
                "cicd.pipeline.run.id",
                "github.commit.sha",
                "github.delivery.id",
                "github.pull_request.number",
                "github.repository.name",
                "github.workflow.job.id",
                "github.workflow.run.attempt",
                "github.workflow.run.id",
            ] {
                assert!(
                    attribute(step, duplicated_identifier).is_none(),
                    "child span duplicated identifier attribute {duplicated_identifier}"
                );
            }
        }
    }

    fn attribute<'span>(span: &'span SpanData, key: &str) -> Option<&'span Value> {
        span.attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == key)
            .map(|attribute| &attribute.value)
    }

    fn attribute_keys(span: &SpanData) -> BTreeSet<&str> {
        span.attributes
            .iter()
            .map(|attribute| attribute.key.as_str())
            .collect()
    }

    fn assert_string_attribute(span: &SpanData, key: &str, expected: &str) {
        assert_eq!(
            attribute(span, key).map(Value::as_str).as_deref(),
            Some(expected),
            "unexpected attribute {key}"
        );
    }

    fn assert_i64_array_attribute(span: &SpanData, key: &str, expected: &[i64]) {
        assert_eq!(
            attribute(span, key),
            Some(&Value::Array(Array::I64(expected.to_vec()))),
            "unexpected attribute {key}"
        );
    }
}
