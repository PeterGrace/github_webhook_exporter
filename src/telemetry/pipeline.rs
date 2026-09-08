//! Bounded pipeline-run summary traces that link one span per job to its own job trace.
//!
//! A completed GitHub Actions run emits one independent root span summarizing the whole run
//! attempt. Each summarized job becomes one direct child span carrying an OpenTelemetry span
//! link to the root span of the `github.actions.job` trace that was exported when that job
//! completed.

use opentelemetry::{
    trace::{
        Link, Span as _, SpanContext, SpanId, SpanKind, TraceContextExt, TraceFlags, TraceId,
        TraceState, Tracer,
    },
    Context, KeyValue,
};
use sha2::{Digest, Sha256};

use crate::{domain::delivery::DeliveryId, security::CanonicalRepositoryName};

use super::{
    trace::{
        delivery_id_attribute, pull_request_numbers_attribute, sentry_description_attribute,
        sentry_operation_attribute, timing_source_attribute, workflow_conclusion_attribute,
        workflow_error_type_attribute, workflow_pipeline_task_run_id_attribute,
        workflow_pipeline_task_run_result_attribute, workflow_run_attempt_attribute,
        workflow_task_name_attribute, workflow_task_run_url_attribute, CommitSha,
        GITHUB_ACTIONS_PIPELINE_OPERATION, GITHUB_ACTIONS_PIPELINE_TASK_OPERATION,
    },
    workflow::{
        append_pipeline_and_repository_context, append_workflow_run_context, DisplayName,
        HistoricalTiming, TimingSource, WorkflowConclusion, WorkflowJobId, WorkflowPullRequests,
        WorkflowRunAttempt, WorkflowRunContext, WorkflowRunId, WorkflowTraceEmitter,
    },
    workflow_error::{PipelineTaskErrorParts, SyntheticWorkflowError},
};

/// The greatest number of job summaries one pipeline-run trace may contain.
///
/// A run attempt reporting more emitted job traces than this is rejected whole, mirroring the
/// step limit that guards job traces: there is never a partial pipeline-run trace.
pub(crate) const MAX_PIPELINE_JOB_SPANS: usize = 256;

const UNNAMED_JOB_NAME: &str = "job";
const UNKNOWN_WORKFLOW_NAME: &str = "workflow";
const PIPELINE_ROOT_REQUIRED_ATTRIBUTE_COUNT: usize = 12;
const PIPELINE_TASK_REQUIRED_ATTRIBUTE_COUNT: usize = 13;

/// The domain separation prefix of the pipeline-run trace identifier preimage.
///
/// It keeps the hashed string from colliding with any other SHA-256 preimage this service or a
/// consumer might derive from the same three GitHub identifiers.
const PIPELINE_TRACE_ID_DOMAIN: &str = "gha-pipeline";

/// The number of leading digest bytes that form a W3C trace identifier.
const TRACE_ID_BYTES: usize = 16;

/// The exported identity of one emitted workflow-job trace root span.
///
/// This is the only part of an emitted job trace that outlives the delivery that produced it; it
/// is what a later pipeline-run summary span links to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WorkflowJobTraceIdentity {
    trace_id: TraceId,
    span_id: SpanId,
}

impl WorkflowJobTraceIdentity {
    /// Creates an exported job trace identity.
    ///
    /// # Parameters
    ///
    /// * `trace_id` - The trace identifier of the emitted job trace.
    /// * `span_id` - The span identifier of the emitted job root span.
    pub(crate) fn new(trace_id: TraceId, span_id: SpanId) -> Self {
        Self { trace_id, span_id }
    }

    /// Returns the trace identifier of the emitted job trace.
    pub(crate) fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    /// Returns the span identifier of the emitted job root span.
    pub(crate) fn span_id(&self) -> SpanId {
        self.span_id
    }

    /// Returns whether both identifiers are valid and can address a real exported span.
    pub(crate) fn is_valid(&self) -> bool {
        self.trace_id != TraceId::INVALID && self.span_id != SpanId::INVALID
    }

    /// Returns the sampled span context used as the link target.
    fn link_span_context(&self) -> SpanContext {
        // `is_remote` is true because the linked span was exported by an earlier delivery, not by
        // the span currently being built. Rust has no implicit conversion here, so the sampled
        // flag must be stated explicitly for collectors to follow the link.
        SpanContext::new(
            self.trace_id,
            self.span_id,
            TraceFlags::SAMPLED,
            true,
            TraceState::NONE,
        )
    }
}

/// One bounded job summary carried by a pipeline-run trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PipelineJobSummary {
    job_id: WorkflowJobId,
    job_name: Option<DisplayName>,
    conclusion: WorkflowConclusion,
    timing: HistoricalTiming,
    identity: WorkflowJobTraceIdentity,
}

impl PipelineJobSummary {
    /// Creates a bounded job summary from already validated components.
    ///
    /// # Parameters
    ///
    /// * `job_id` - The validated workflow job identifier.
    /// * `job_name` - The optional sanitized job name.
    /// * `conclusion` - The bounded job conclusion.
    /// * `timing` - The historical interval selected when the job trace was emitted.
    /// * `identity` - The exported identity of that job trace.
    pub(crate) fn new(
        job_id: WorkflowJobId,
        job_name: Option<DisplayName>,
        conclusion: WorkflowConclusion,
        timing: HistoricalTiming,
        identity: WorkflowJobTraceIdentity,
    ) -> Self {
        Self {
            job_id,
            job_name,
            conclusion,
            timing,
            identity,
        }
    }

    /// Returns the bounded job conclusion.
    pub(crate) fn conclusion(&self) -> WorkflowConclusion {
        self.conclusion
    }

    /// Returns the historical interval of the summarized job.
    pub(crate) fn timing(&self) -> &HistoricalTiming {
        &self.timing
    }

    fn name(&self) -> &str {
        self.job_name
            .as_ref()
            .map_or(UNNAMED_JOB_NAME, DisplayName::as_str)
    }
}

/// The validated parts needed to construct a pipeline-run summary trace.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PipelineRunTraceParts {
    pub(crate) repository_name: CanonicalRepositoryName,
    pub(crate) delivery_id: DeliveryId,
    pub(crate) workflow_name: Option<DisplayName>,
    pub(crate) head_sha: Option<CommitSha>,
    pub(crate) pull_requests: WorkflowPullRequests,
    pub(crate) run_context: WorkflowRunContext,
    pub(crate) jobs: Vec<PipelineJobSummary>,
}

/// An owned pipeline-run summary trace accepted by the telemetry emitter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PipelineRunTrace {
    repository_name: CanonicalRepositoryName,
    delivery_id: DeliveryId,
    workflow_name: Option<DisplayName>,
    head_sha: Option<CommitSha>,
    pull_requests: WorkflowPullRequests,
    run_context: WorkflowRunContext,
    conclusion: WorkflowConclusion,
    timing: HistoricalTiming,
    jobs: Vec<PipelineJobSummary>,
}

impl PipelineRunTrace {
    /// Creates a pipeline-run summary trace whose interval and conclusion derive from its jobs.
    ///
    /// # Parameters
    ///
    /// * `parts` - The already validated pipeline-run components.
    ///
    /// # Returns
    ///
    /// A bounded pipeline-run trace, or `None` when `parts.jobs` is empty or holds more than
    /// [`MAX_PIPELINE_JOB_SPANS`] summaries. A run with nothing to summarize emits no trace.
    pub(crate) fn new(parts: PipelineRunTraceParts) -> Option<Self> {
        if parts.jobs.is_empty() || parts.jobs.len() > MAX_PIPELINE_JOB_SPANS {
            return None;
        }
        let timing = aggregate_timing(&parts.jobs)?;
        let conclusion = aggregate_conclusion(&parts.jobs);

        Some(Self {
            repository_name: parts.repository_name,
            delivery_id: parts.delivery_id,
            workflow_name: parts.workflow_name,
            head_sha: parts.head_sha,
            pull_requests: parts.pull_requests,
            run_context: parts.run_context,
            conclusion,
            timing,
            jobs: parts.jobs,
        })
    }

    /// Returns the aggregated run conclusion derived from the summarized jobs.
    pub(crate) fn conclusion(&self) -> WorkflowConclusion {
        self.conclusion
    }

    /// Returns the derived historical interval spanning every summarized job.
    pub(crate) fn timing(&self) -> &HistoricalTiming {
        &self.timing
    }

    /// Returns the bounded job summaries carried by this trace.
    pub(crate) fn jobs(&self) -> &[PipelineJobSummary] {
        &self.jobs
    }

    fn workflow_name(&self) -> &str {
        self.workflow_name
            .as_ref()
            .map_or(UNKNOWN_WORKFLOW_NAME, DisplayName::as_str)
    }
}

impl WorkflowTraceEmitter {
    /// Emits one independent pipeline-run summary span and its per-job child spans.
    ///
    /// Each child links to the root span of the job trace exported for that job. Every failing
    /// child additionally carries one bounded `exception` span event and one run-scoped synthetic
    /// error, because Sentry renders a span as errored only when an error event references it;
    /// the OpenTelemetry status alone leaves the span uncolored and unlinked. Those run-scoped
    /// errors group separately from the job- and step-scoped errors raised on the job traces.
    ///
    /// The root itself raises nothing: its conclusion is the most severe job conclusion, so a
    /// failing root always has a failing child that already explains it.
    ///
    /// # Parameters
    ///
    /// * `pipeline` - The bounded pipeline-run summary trace to export.
    pub(crate) fn emit_pipeline(&self, pipeline: &PipelineRunTrace) {
        let Some(tracer) = self.tracer() else {
            return;
        };

        let mut root = tracer.build_with_context(
            tracer
                .span_builder(pipeline.workflow_name().to_owned())
                .with_kind(SpanKind::Internal)
                .with_start_time(pipeline.timing().start())
                .with_attributes(pipeline_root_attributes(pipeline)),
            &pipeline_root_context(pipeline),
        );
        root.set_status(pipeline.conclusion().status());
        let parent_context = Context::current_with_span(root);

        for job in pipeline.jobs() {
            let span_name = pipeline_task_span_name(pipeline, job);
            let mut span = tracer.build_with_context(
                tracer
                    .span_builder(span_name.clone())
                    .with_kind(SpanKind::Internal)
                    .with_start_time(job.timing().start())
                    .with_links(vec![Link::with_context(job.identity.link_span_context())])
                    .with_attributes(pipeline_task_attributes(pipeline, job)),
                &parent_context,
            );
            span.set_status(job.conclusion().status());
            if job.conclusion().emits_synthetic_error() {
                let span_context = span.span_context();
                let error = SyntheticWorkflowError::for_pipeline_task(PipelineTaskErrorParts {
                    repository_name: pipeline.repository_name.as_str(),
                    workflow_name: pipeline.workflow_name(),
                    job_name: job.job_name.as_ref().map(DisplayName::as_str),
                    job_id: job.job_id,
                    conclusion: job.conclusion,
                    span_description: span_name,
                    timestamp: job.timing().end(),
                    trace_id: span_context.trace_id(),
                    span_id: span_context.span_id(),
                });
                span.add_event_with_timestamp(
                    "exception",
                    error.timestamp(),
                    error.span_event_attributes().into(),
                );
                if let Some(reporter) = self.error_reporter() {
                    reporter.report(error);
                }
            }
            span.end_with_timestamp(job.timing().end());
        }

        parent_context
            .span()
            .end_with_timestamp(pipeline.timing().end());
    }
}

/// Returns the parent context that pins one pipeline-run root span to its derived trace.
///
/// `SpanBuilder` exposes no trace identifier of its own and `IdGenerator::new_trace_id` receives
/// no per-span context, so the only way to preset a root's trace is through the parent context
/// passed to `build_with_context`. Two details of that context are load bearing:
///
/// * The span identifier is [`SpanId::INVALID`]. The SDK copies the parent's span identifier into
///   `parent_span_id`, so an invalid one leaves the emitted span a true root, byte-for-byte as it
///   was when built from an empty context.
/// * The trace flags are [`TraceFlags::SAMPLED`]. The default `ParentBased` sampler follows any
///   parent present in the context, so an unsampled synthetic parent would drop the whole trace.
///
/// Falls back to an empty context, and therefore to a randomly generated trace identifier, when
/// the digest cannot address a real trace.
///
/// # Parameters
///
/// * `pipeline` - The bounded pipeline-run summary trace about to be exported.
fn pipeline_root_context(pipeline: &PipelineRunTrace) -> Context {
    let Some(trace_id) = derive_pipeline_trace_id(
        &pipeline.repository_name,
        pipeline.run_context.run_id(),
        pipeline.run_context.run_attempt(),
    ) else {
        return Context::new();
    };

    // `is_remote` is false: this context stands in for the absent parent of a locally built root,
    // not for a span context received from another service.
    Context::new().with_remote_span_context(SpanContext::new(
        trace_id,
        SpanId::INVALID,
        TraceFlags::SAMPLED,
        false,
        TraceState::NONE,
    ))
}

/// Derives the deterministic trace identifier of one pipeline-run summary trace.
///
/// The identifier is the first 16 bytes of
/// `sha256("gha-pipeline:{repository}:{run_id}:{run_attempt}")`, where `{repository}` is the
/// canonical lowercase `owner/repository` name and the two identifiers are decimal. Every input is
/// available to a running job as `$GITHUB_REPOSITORY` (lowercased), `$GITHUB_RUN_ID`, and
/// `$GITHUB_RUN_ATTEMPT`, so telemetry emitted from inside the run can compute the same value and
/// attach natively to this trace long before the completed `workflow_run` webhook mints it.
///
/// # Parameters
///
/// * `repository_name` - The canonical repository name, which scopes run identifiers that GitHub
///   only guarantees to be unique per repository.
/// * `run_id` - The validated workflow run identifier.
/// * `run_attempt` - The validated workflow run attempt.
///
/// # Returns
///
/// The derived trace identifier, or [`None`] when the digest truncates to [`TraceId::INVALID`] and
/// the caller must fall back to a randomly generated identifier.
fn derive_pipeline_trace_id(
    repository_name: &CanonicalRepositoryName,
    run_id: WorkflowRunId,
    run_attempt: WorkflowRunAttempt,
) -> Option<TraceId> {
    // The preimage is built once per completed run and is the exact byte sequence a consumer must
    // reproduce, so it is written out literally rather than fed to the digest piecewise.
    let preimage = format!(
        "{PIPELINE_TRACE_ID_DOMAIN}:{}:{}:{}",
        repository_name.as_str(),
        run_id.get(),
        run_attempt.get()
    );
    let digest = Sha256::digest(preimage.as_bytes());
    let mut trace_bytes = [0_u8; TRACE_ID_BYTES];
    trace_bytes.copy_from_slice(&digest[..TRACE_ID_BYTES]);

    let trace_id = TraceId::from_bytes(trace_bytes);
    (trace_id != TraceId::INVALID).then_some(trace_id)
}

fn pipeline_root_attributes(pipeline: &PipelineRunTrace) -> Vec<KeyValue> {
    let mut attributes = Vec::with_capacity(
        PIPELINE_ROOT_REQUIRED_ATTRIBUTE_COUNT
            + usize::from(pipeline.head_sha.is_some())
            + usize::from(pipeline.conclusion.error_type().is_some())
            + usize::from(!pipeline.pull_requests.as_slice().is_empty())
            + usize::from(pipeline.run_context.source_branch().is_some())
            + usize::from(pipeline.run_context.target_branch().is_some()),
    );
    append_pipeline_context(&mut attributes, pipeline);
    attributes.push(delivery_id_attribute(&pipeline.delivery_id));
    attributes.push(workflow_run_attempt_attribute(
        pipeline.run_context.run_attempt(),
    ));
    attributes.push(workflow_conclusion_attribute(pipeline.conclusion));
    if let Some(error_type) = workflow_error_type_attribute(pipeline.conclusion) {
        attributes.push(error_type);
    }
    if let Some(pull_request_numbers) =
        pull_request_numbers_attribute(pipeline.pull_requests.as_slice())
    {
        attributes.push(pull_request_numbers);
    }
    append_workflow_run_context(&mut attributes, Some(&pipeline.run_context));
    attributes.push(sentry_operation_attribute(
        GITHUB_ACTIONS_PIPELINE_OPERATION,
    ));
    attributes.push(sentry_description_attribute(
        pipeline.workflow_name().to_owned(),
    ));
    attributes.push(timing_source_attribute(pipeline.timing.source()));
    attributes
}

fn pipeline_task_attributes(
    pipeline: &PipelineRunTrace,
    job: &PipelineJobSummary,
) -> Vec<KeyValue> {
    let mut attributes = Vec::with_capacity(
        PIPELINE_TASK_REQUIRED_ATTRIBUTE_COUNT
            + usize::from(pipeline.head_sha.is_some())
            + usize::from(job.conclusion.error_type().is_some())
            + usize::from(pipeline.run_context.source_branch().is_some())
            + usize::from(pipeline.run_context.target_branch().is_some()),
    );
    append_pipeline_context(&mut attributes, pipeline);
    attributes.push(workflow_task_name_attribute(job.name()));
    attributes.push(workflow_pipeline_task_run_id_attribute(job.job_id));
    attributes.push(workflow_conclusion_attribute(job.conclusion));
    attributes.push(workflow_pipeline_task_run_result_attribute(job.conclusion));
    if let Some(error_type) = workflow_error_type_attribute(job.conclusion) {
        attributes.push(error_type);
    }
    append_workflow_run_context(&mut attributes, Some(&pipeline.run_context));
    attributes.push(workflow_task_run_url_attribute(
        &pipeline.repository_name,
        pipeline.run_context.run_id(),
        job.job_id,
    ));
    attributes.push(sentry_operation_attribute(
        GITHUB_ACTIONS_PIPELINE_TASK_OPERATION,
    ));
    attributes.push(sentry_description_attribute(pipeline_task_span_name(
        pipeline, job,
    )));
    attributes.push(timing_source_attribute(job.timing.source()));
    attributes
}

fn append_pipeline_context(attributes: &mut Vec<KeyValue>, pipeline: &PipelineRunTrace) {
    append_pipeline_and_repository_context(
        attributes,
        pipeline.workflow_name(),
        &pipeline.repository_name,
        pipeline.run_context.run_id(),
        pipeline.head_sha.as_ref(),
    );
}

fn pipeline_task_span_name(pipeline: &PipelineRunTrace, job: &PipelineJobSummary) -> String {
    format!("{} / {}", pipeline.workflow_name(), job.name())
}

/// Returns the interval spanning every summarized job.
///
/// The interval is `reported` only when every summarized job used reported timing, because a
/// single fallback job makes the derived run boundary approximate.
fn aggregate_timing(jobs: &[PipelineJobSummary]) -> Option<HistoricalTiming> {
    let start = jobs.iter().map(|job| job.timing.start()).min()?;
    let end = jobs.iter().map(|job| job.timing.end()).max()?;
    let source = if jobs
        .iter()
        .all(|job| job.timing.source() == TimingSource::Reported)
    {
        TimingSource::Reported
    } else {
        TimingSource::Fallback
    };
    // `start <= end` holds because both bounds come from the same ordered job intervals; the
    // fallback keeps this function total rather than relying on that invariant.
    Some(
        HistoricalTiming::derived(start, end, source)
            .unwrap_or_else(|_| HistoricalTiming::fallback(end)),
    )
}

/// Reduces the summarized job conclusions into one bounded run conclusion.
///
/// Severity descends `failure`, `timed_out`, `cancelled`, `other`, `neutral`, `success`,
/// `skipped`, so a failed or timed-out job always decides the run and skipped jobs never mask an
/// otherwise successful one.
fn aggregate_conclusion(jobs: &[PipelineJobSummary]) -> WorkflowConclusion {
    jobs.iter()
        .map(|job| job.conclusion)
        .max_by_key(|conclusion| run_severity(*conclusion))
        .unwrap_or(WorkflowConclusion::Other)
}

const fn run_severity(conclusion: WorkflowConclusion) -> u8 {
    match conclusion {
        WorkflowConclusion::Skipped => 0,
        WorkflowConclusion::Success => 1,
        WorkflowConclusion::Neutral => 2,
        WorkflowConclusion::Other => 3,
        WorkflowConclusion::Cancelled => 4,
        WorkflowConclusion::TimedOut => 5,
        WorkflowConclusion::Failure => 6,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Arc, Mutex},
        time::{Duration, SystemTime},
    };

    use opentelemetry::{
        trace::{SpanId, SpanKind, Status, TraceId, TracerProvider as _},
        Value,
    };
    use opentelemetry_sdk::{
        error::OTelSdkResult,
        trace::{SdkTracerProvider, SimpleSpanProcessor, SpanData, SpanExporter},
    };

    use super::{
        derive_pipeline_trace_id, PipelineJobSummary, PipelineRunTrace, PipelineRunTraceParts,
        SyntheticWorkflowError, WorkflowJobTraceIdentity, MAX_PIPELINE_JOB_SPANS,
    };
    use crate::{
        domain::{delivery::DeliveryId, merge_queue::PullRequestNumber},
        security::CanonicalRepositoryName,
        telemetry::{
            trace::CommitSha,
            workflow::{
                DisplayName, HistoricalTiming, TimingSource, WorkflowBranch, WorkflowConclusion,
                WorkflowEvent, WorkflowJobId, WorkflowPullRequests, WorkflowRunAttempt,
                WorkflowRunContext, WorkflowRunId, WorkflowTraceEmitter,
            },
            workflow_error::WorkflowErrorReporter,
        },
    };

    const REPOSITORY: &str = "owner/repository";
    const DELIVERY: &str = "550e8400-e29b-41d4-a716-446655440000";
    const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

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

    fn identity(seed: u128) -> WorkflowJobTraceIdentity {
        WorkflowJobTraceIdentity::new(
            TraceId::from(seed),
            SpanId::from_bytes((seed as u64).to_be_bytes()),
        )
    }

    fn instant(seconds: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
    }

    fn job(
        job_id: i64,
        name: Option<&str>,
        conclusion: WorkflowConclusion,
        start_seconds: u64,
        end_seconds: u64,
    ) -> PipelineJobSummary {
        PipelineJobSummary::new(
            WorkflowJobId::new(job_id).expect("job id is positive"),
            name.and_then(DisplayName::sanitize),
            conclusion,
            HistoricalTiming::reported(instant(start_seconds), instant(end_seconds))
                .expect("timing is ordered"),
            identity(u128::try_from(job_id).expect("job id fits a trace seed")),
        )
    }

    fn fallback_job(job_id: i64, instant_seconds: u64) -> PipelineJobSummary {
        PipelineJobSummary::new(
            WorkflowJobId::new(job_id).expect("job id is positive"),
            None,
            WorkflowConclusion::Success,
            HistoricalTiming::fallback(instant(instant_seconds)),
            identity(u128::try_from(job_id).expect("job id fits a trace seed")),
        )
    }

    fn run_context() -> WorkflowRunContext {
        WorkflowRunContext::new(
            WorkflowRunId::new(31).expect("run id is positive"),
            WorkflowRunAttempt::new(2).expect("attempt is positive"),
            WorkflowEvent::MergeGroup,
            WorkflowBranch::sanitize("gh-readonly-queue/main/pr-7"),
            WorkflowBranch::sanitize("main"),
        )
    }

    fn repository(full_name: &str) -> CanonicalRepositoryName {
        CanonicalRepositoryName::new(full_name).expect("repository name is canonical")
    }

    fn derived_trace_id(full_name: &str, run_id: i64, run_attempt: i64) -> TraceId {
        derive_pipeline_trace_id(
            &repository(full_name),
            WorkflowRunId::new(run_id).expect("run id is positive"),
            WorkflowRunAttempt::new(run_attempt).expect("attempt is positive"),
        )
        .expect("the fixture digest addresses a real trace")
    }

    fn parts(jobs: Vec<PipelineJobSummary>) -> PipelineRunTraceParts {
        PipelineRunTraceParts {
            repository_name: CanonicalRepositoryName::new(REPOSITORY)
                .expect("repository name is canonical"),
            delivery_id: DeliveryId::parse(DELIVERY).expect("delivery id parses"),
            workflow_name: DisplayName::sanitize("Build Workflow"),
            head_sha: CommitSha::parse(HEAD_SHA).ok(),
            pull_requests: WorkflowPullRequests::new(
                [7, 9]
                    .into_iter()
                    .filter_map(|number| PullRequestNumber::new(number).ok()),
            ),
            run_context: run_context(),
            jobs,
        }
    }

    /// One synthetic error captured from a pipeline-run emission.
    #[derive(Clone, Debug, PartialEq, Eq)]
    struct ReportedError {
        exception_type: String,
        description: String,
        fingerprint: [String; 7],
    }

    /// Captures every synthetic error a pipeline-run emission reports.
    #[derive(Clone, Default, Debug)]
    struct CollectingErrorReporter(Arc<Mutex<Vec<ReportedError>>>);

    impl CollectingErrorReporter {
        fn reported(&self) -> Vec<ReportedError> {
            self.0
                .lock()
                .expect("error capture lock is available")
                .clone()
        }
    }

    impl WorkflowErrorReporter for CollectingErrorReporter {
        fn report(&self, error: SyntheticWorkflowError) {
            self.0
                .lock()
                .expect("error capture lock is available")
                .push(ReportedError {
                    exception_type: error.exception_type().to_owned(),
                    description: error.description().to_owned(),
                    fingerprint: error.fingerprint().map(ToOwned::to_owned),
                });
        }
    }

    fn emitted_spans(pipeline: &PipelineRunTrace) -> Vec<SpanData> {
        emitted_spans_and_errors(pipeline).0
    }

    fn emitted_spans_and_errors(
        pipeline: &PipelineRunTrace,
    ) -> (Vec<SpanData>, Vec<ReportedError>) {
        let exporter = CollectingSpanExporter::default();
        let reporter = CollectingErrorReporter::default();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
            .build();
        let emitter = WorkflowTraceEmitter::new_with_error_reporter(
            provider.tracer("github_webhook_exporter"),
            Arc::new(reporter.clone()),
        );

        emitter.emit_pipeline(pipeline);
        provider.force_flush().expect("spans flush");
        (exporter.finished_spans(), reporter.reported())
    }

    fn exception_events(span: &SpanData) -> Vec<(String, String)> {
        span.events
            .iter()
            .filter(|event| event.name == "exception")
            .map(|event| {
                let value = |key: &str| {
                    event
                        .attributes
                        .iter()
                        .find(|attribute| attribute.key.as_str() == key)
                        .map(|attribute| attribute.value.as_str().into_owned())
                        .unwrap_or_default()
                };
                (value("exception.type"), value("exception.message"))
            })
            .collect()
    }

    fn attribute<'span>(span: &'span SpanData, key: &str) -> Option<&'span Value> {
        span.attributes
            .iter()
            .find(|attribute| attribute.key.as_str() == key)
            .map(|attribute| &attribute.value)
    }

    fn string_attribute(span: &SpanData, key: &str) -> Option<String> {
        attribute(span, key).map(|value| value.as_str().into_owned())
    }

    #[test]
    fn a_run_without_summarized_jobs_emits_no_pipeline_trace() {
        assert!(PipelineRunTrace::new(parts(Vec::new())).is_none());
    }

    #[test]
    fn a_run_over_the_job_limit_is_rejected_whole() {
        let within_limit = (1..=MAX_PIPELINE_JOB_SPANS)
            .map(|index| {
                job(
                    i64::try_from(index).expect("index fits an identifier"),
                    None,
                    WorkflowConclusion::Success,
                    0,
                    1,
                )
            })
            .collect::<Vec<_>>();
        let mut over_limit = within_limit.clone();
        over_limit.push(job(
            i64::try_from(MAX_PIPELINE_JOB_SPANS + 1).expect("index fits an identifier"),
            None,
            WorkflowConclusion::Success,
            0,
            1,
        ));

        assert!(PipelineRunTrace::new(parts(within_limit)).is_some());
        assert!(PipelineRunTrace::new(parts(over_limit)).is_none());
    }

    #[test]
    fn run_conclusion_takes_the_most_severe_job_and_skipped_never_masks_success() {
        let cases = [
            (
                vec![WorkflowConclusion::Success, WorkflowConclusion::Skipped],
                WorkflowConclusion::Success,
                Status::Ok,
            ),
            (
                vec![WorkflowConclusion::Success, WorkflowConclusion::Failure],
                WorkflowConclusion::Failure,
                Status::error("workflow_failed"),
            ),
            (
                vec![WorkflowConclusion::TimedOut, WorkflowConclusion::Failure],
                WorkflowConclusion::Failure,
                Status::error("workflow_failed"),
            ),
            (
                vec![WorkflowConclusion::Success, WorkflowConclusion::TimedOut],
                WorkflowConclusion::TimedOut,
                Status::error("workflow_failed"),
            ),
            (
                vec![WorkflowConclusion::Success, WorkflowConclusion::Cancelled],
                WorkflowConclusion::Cancelled,
                Status::Unset,
            ),
            (
                vec![WorkflowConclusion::Skipped, WorkflowConclusion::Skipped],
                WorkflowConclusion::Skipped,
                Status::Unset,
            ),
        ];

        for (conclusions, expected, expected_status) in cases {
            let jobs = conclusions
                .iter()
                .enumerate()
                .map(|(index, conclusion)| {
                    job(
                        i64::try_from(index + 1).expect("index fits an identifier"),
                        None,
                        *conclusion,
                        0,
                        1,
                    )
                })
                .collect::<Vec<_>>();
            let pipeline =
                PipelineRunTrace::new(parts(jobs)).expect("a summarized run builds a trace");
            assert_eq!(pipeline.conclusion(), expected);
            assert_eq!(pipeline.conclusion().status(), expected_status);
        }
    }

    #[test]
    fn run_interval_spans_every_job_and_degrades_to_fallback_timing() {
        let reported = PipelineRunTrace::new(parts(vec![
            job(1, None, WorkflowConclusion::Success, 100, 400),
            job(2, None, WorkflowConclusion::Success, 50, 200),
        ]))
        .expect("a summarized run builds a trace");
        assert_eq!(reported.timing().start(), instant(50));
        assert_eq!(reported.timing().end(), instant(400));
        assert_eq!(reported.timing().source(), TimingSource::Reported);

        let degraded = PipelineRunTrace::new(parts(vec![
            job(1, None, WorkflowConclusion::Success, 100, 400),
            fallback_job(2, 50),
        ]))
        .expect("a summarized run builds a trace");
        assert_eq!(degraded.timing().start(), instant(50));
        assert_eq!(degraded.timing().end(), instant(400));
        assert_eq!(degraded.timing().source(), TimingSource::Fallback);
    }

    /// The shared fixture both this exporter and any in-job emitter must reproduce.
    ///
    /// Changing either value is a breaking change to the association contract documented in
    /// `book/src/reference/traces.md`, not a test detail.
    #[test]
    fn the_derived_trace_id_matches_the_published_shared_fixture() {
        assert_eq!(
            format!("{:032x}", derived_trace_id("owner/repository", 31, 2)),
            "744f46e18e520d0324ebe726e54e6861"
        );
        assert_eq!(
            format!(
                "{:032x}",
                derived_trace_id("stackblitz/bolt", 12_345_678_901, 1)
            ),
            "18f1e9fd4498a37448e4d9a574bd3645"
        );
    }

    #[test]
    fn the_derived_trace_id_separates_repositories_runs_and_attempts() {
        let baseline = derived_trace_id("owner/repository", 31, 2);

        assert_ne!(baseline, derived_trace_id("owner/other-repository", 31, 2));
        assert_ne!(baseline, derived_trace_id("owner/repository", 32, 2));
        assert_ne!(baseline, derived_trace_id("owner/repository", 31, 1));
        assert_eq!(baseline, derived_trace_id("owner/repository", 31, 2));
    }

    /// The consumer hashes `$GITHUB_REPOSITORY` lowercased, so a mixed-case payload must not move
    /// the trace: `CanonicalRepositoryName` is what reaches the digest.
    #[test]
    fn the_derived_trace_id_ignores_repository_name_casing_and_padding() {
        assert_eq!(
            derived_trace_id("owner/repository", 31, 2),
            derived_trace_id("  Owner/Repository  ", 31, 2)
        );
    }

    #[test]
    fn the_exported_root_carries_the_derived_trace_id_and_stays_a_root() {
        let pipeline = PipelineRunTrace::new(parts(vec![
            job(41, Some("Linux Job"), WorkflowConclusion::Success, 100, 300),
            job(42, None, WorkflowConclusion::Failure, 200, 400),
        ]))
        .expect("a summarized run builds a trace");
        let expected = derived_trace_id(REPOSITORY, 31, 2);

        let spans = emitted_spans(&pipeline);
        let root = spans
            .iter()
            .find(|span| span.parent_span_id == SpanId::INVALID)
            .expect("one independent pipeline root is exported");

        assert_eq!(root.span_context.trace_id(), expected);
        assert_ne!(root.span_context.trace_id(), TraceId::INVALID);
        assert_ne!(root.span_context.span_id(), SpanId::INVALID);
        // The synthetic parent context must not survive into the exported span: a preset trace
        // that also introduced a parent or unsampled the trace would change the emitted shape.
        assert!(!root.parent_span_is_remote);
        assert!(root.span_context.is_sampled());
        assert!(spans
            .iter()
            .all(|span| span.span_context.trace_id() == expected));
    }

    #[test]
    fn a_re_emitted_run_reuses_its_trace_while_a_re_run_gets_its_own() {
        let pipeline = PipelineRunTrace::new(parts(vec![job(
            41,
            Some("Linux Job"),
            WorkflowConclusion::Success,
            100,
            300,
        )]))
        .expect("a summarized run builds a trace");

        let first = emitted_spans(&pipeline);
        let second = emitted_spans(&pipeline);

        assert_eq!(
            first[0].span_context.trace_id(),
            second[0].span_context.trace_id()
        );
        assert_ne!(
            first[0].span_context.trace_id(),
            derived_trace_id(REPOSITORY, 31, 3)
        );
    }

    #[test]
    fn emitter_exports_one_root_and_one_linked_child_per_job() {
        let pipeline = PipelineRunTrace::new(parts(vec![
            job(
                41,
                Some("Linux\tJob"),
                WorkflowConclusion::Success,
                100,
                300,
            ),
            job(42, None, WorkflowConclusion::Failure, 200, 400),
        ]))
        .expect("a summarized run builds a trace");

        let spans = emitted_spans(&pipeline);
        assert_eq!(spans.len(), 3);
        let root = spans
            .iter()
            .find(|span| span.parent_span_id == SpanId::INVALID)
            .expect("one independent pipeline root is exported");
        let children = spans
            .iter()
            .filter(|span| span.parent_span_id == root.span_context.span_id())
            .collect::<Vec<_>>();
        assert_eq!(children.len(), 2);
        assert!(spans
            .iter()
            .all(|span| span.span_kind == SpanKind::Internal));
        assert!(children
            .iter()
            .all(|child| child.span_context.trace_id() == root.span_context.trace_id()));

        assert_eq!(root.name, "Build Workflow");
        assert_eq!(root.status, Status::error("workflow_failed"));
        assert_eq!(root.start_time, instant(100));
        assert_eq!(root.end_time, instant(400));
        assert_eq!(
            string_attribute(root, "sentry.op").as_deref(),
            Some("github.actions.pipeline")
        );
        assert_eq!(
            string_attribute(root, "sentry.description").as_deref(),
            Some("Build Workflow")
        );
        assert_eq!(
            string_attribute(root, "github.workflow.conclusion").as_deref(),
            Some("failure")
        );
        assert_eq!(
            string_attribute(root, "error.type").as_deref(),
            Some("GitHubActionsTaskFailure")
        );
        assert_eq!(
            string_attribute(root, "cicd.pipeline.run.id").as_deref(),
            Some("31")
        );
        assert_eq!(
            string_attribute(root, "github.workflow.run.attempt").as_deref(),
            Some("2")
        );
        assert_eq!(
            string_attribute(root, "github.workflow.event").as_deref(),
            Some("merge_group")
        );
        assert_eq!(
            string_attribute(root, "vcs.ref.head.revision").as_deref(),
            Some(HEAD_SHA)
        );
        assert_eq!(
            string_attribute(root, "github.delivery.id").as_deref(),
            Some(DELIVERY)
        );
        assert!(root.links.iter().next().is_none());
        assert!(attribute(root, "cicd.pipeline.task.run.id").is_none());

        let succeeded = children
            .iter()
            .find(|child| child.name == "Build Workflow / LinuxJob")
            .expect("the named job summary is exported");
        assert_eq!(succeeded.status, Status::Ok);
        assert_eq!(succeeded.start_time, instant(100));
        assert_eq!(succeeded.end_time, instant(300));
        assert_eq!(
            string_attribute(succeeded, "sentry.op").as_deref(),
            Some("github.actions.pipeline.task")
        );
        assert_eq!(
            string_attribute(succeeded, "cicd.pipeline.task.name").as_deref(),
            Some("LinuxJob")
        );
        assert_eq!(
            string_attribute(succeeded, "cicd.pipeline.task.run.id").as_deref(),
            Some("41")
        );
        assert_eq!(
            string_attribute(succeeded, "cicd.pipeline.task.run.result").as_deref(),
            Some("success")
        );
        assert_eq!(
            string_attribute(succeeded, "cicd.pipeline.task.run.url.full").as_deref(),
            Some(&*format!(
                "https://github.com/{REPOSITORY}/actions/runs/31/job/41"
            ))
        );
        assert!(attribute(succeeded, "github.delivery.id").is_none());

        let unnamed = children
            .iter()
            .find(|child| child.name == "Build Workflow / job")
            .expect("the unnamed job summary uses the fixed fallback");
        assert_eq!(unnamed.status, Status::error("workflow_failed"));
        assert_eq!(
            string_attribute(unnamed, "error.type").as_deref(),
            Some("GitHubActionsTaskFailure")
        );

        let links = children
            .iter()
            .map(|child| {
                let link = child
                    .links
                    .iter()
                    .next()
                    .expect("every job summary links to its job trace");
                assert_eq!(child.links.len(), 1);
                (link.span_context.trace_id(), link.span_context.span_id())
            })
            .collect::<Vec<_>>();
        for (job_id, link) in [41_u128, 42].into_iter().zip(links) {
            let expected = identity(job_id);
            assert_eq!(link, (expected.trace_id(), expected.span_id()));
        }
    }

    #[test]
    fn failing_summaries_carry_one_bounded_exception_event_and_one_run_scoped_error() {
        let pipeline = PipelineRunTrace::new(parts(vec![
            job(41, Some("Linux Job"), WorkflowConclusion::Success, 100, 300),
            job(42, None, WorkflowConclusion::Failure, 200, 400),
            job(43, Some("Slow Job"), WorkflowConclusion::TimedOut, 150, 350),
            job(
                44,
                Some("Skipped Job"),
                WorkflowConclusion::Cancelled,
                120,
                320,
            ),
        ]))
        .expect("a summarized run builds a trace");

        let (spans, reported) = emitted_spans_and_errors(&pipeline);
        let root = spans
            .iter()
            .find(|span| span.parent_span_id == SpanId::INVALID)
            .expect("one independent pipeline root is exported");
        // The root conclusion is the most severe child conclusion, so a failing child always
        // explains it and the root never raises its own error.
        assert_eq!(root.status, Status::error("workflow_failed"));
        assert!(exception_events(root).is_empty());

        let child = |name: &str| {
            spans
                .iter()
                .find(|span| span.name == name)
                .expect("the summarized job is exported")
        };
        assert!(exception_events(child("Build Workflow / Linux Job")).is_empty());
        assert!(exception_events(child("Build Workflow / Skipped Job")).is_empty());
        assert_eq!(
            exception_events(child("Build Workflow / job")),
            vec![(
                "GitHubActionsTaskFailure".to_owned(),
                "CI run job failed: task 42".to_owned()
            )]
        );
        assert_eq!(
            exception_events(child("Build Workflow / Slow Job")),
            vec![(
                "GitHubActionsTaskTimeout".to_owned(),
                "CI run job timed out: Slow Job".to_owned()
            )]
        );

        assert_eq!(
            reported,
            vec![
                ReportedError {
                    exception_type: "GitHubActionsTaskFailure".to_owned(),
                    description: "CI run job failed: task 42".to_owned(),
                    fingerprint: [
                        "github-actions-task",
                        "pipeline-task",
                        REPOSITORY,
                        "Build Workflow",
                        "unnamed-job",
                        "unnamed-job",
                        "failure",
                    ]
                    .map(ToOwned::to_owned),
                },
                ReportedError {
                    exception_type: "GitHubActionsTaskTimeout".to_owned(),
                    description: "CI run job timed out: Slow Job".to_owned(),
                    fingerprint: [
                        "github-actions-task",
                        "pipeline-task",
                        REPOSITORY,
                        "Build Workflow",
                        "Slow Job",
                        "Slow Job",
                        "timed_out",
                    ]
                    .map(ToOwned::to_owned),
                },
            ]
        );
    }

    #[test]
    fn a_successful_run_reports_no_errors_and_an_emitter_without_a_reporter_still_marks_spans() {
        let successful = PipelineRunTrace::new(parts(vec![job(
            41,
            Some("Linux Job"),
            WorkflowConclusion::Success,
            100,
            300,
        )]))
        .expect("a summarized run builds a trace");
        assert!(emitted_spans_and_errors(&successful).1.is_empty());

        // Without a Sentry client the span event is still the only failure marker on the span.
        let exporter = CollectingSpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
            .build();
        let failed = PipelineRunTrace::new(parts(vec![job(
            42,
            Some("Linux Job"),
            WorkflowConclusion::Failure,
            100,
            300,
        )]))
        .expect("a summarized run builds a trace");
        WorkflowTraceEmitter::new(provider.tracer("github_webhook_exporter"))
            .emit_pipeline(&failed);
        provider.force_flush().expect("spans flush");
        let spans = exporter.finished_spans();
        let child = spans
            .iter()
            .find(|span| span.name == "Build Workflow / Linux Job")
            .expect("the summarized job is exported");
        assert_eq!(
            exception_events(child),
            vec![(
                "GitHubActionsTaskFailure".to_owned(),
                "CI run job failed: Linux Job".to_owned()
            )]
        );
    }

    #[test]
    fn a_disabled_emitter_exports_no_pipeline_spans() {
        let pipeline = PipelineRunTrace::new(parts(vec![job(
            41,
            None,
            WorkflowConclusion::Success,
            100,
            300,
        )]))
        .expect("a summarized run builds a trace");

        // A disabled emitter has no tracer at all, so this must be a no-op rather than a panic.
        WorkflowTraceEmitter::disabled().emit_pipeline(&pipeline);
    }

    #[test]
    fn zero_valued_job_identities_are_rejected() {
        assert!(!WorkflowJobTraceIdentity::new(TraceId::INVALID, SpanId::from(7)).is_valid());
        assert!(!WorkflowJobTraceIdentity::new(TraceId::from(7), SpanId::INVALID).is_valid());
        assert!(identity(7).is_valid());
    }
}
