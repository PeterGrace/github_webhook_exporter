use std::{borrow::Cow, fmt, sync::Arc, time::SystemTime};

use opentelemetry::{
    trace::{SpanId, TraceId},
    KeyValue,
};
use sentry::{
    protocol::{Context, Event, Exception, Mechanism, SpanStatus, TraceContext},
    Client, Level,
};

use super::workflow::{WorkflowConclusion, WorkflowJobTrace, WorkflowStepTrace};

const UNKNOWN_WORKFLOW_NAME: &str = "workflow";
const UNNAMED_JOB_GROUPING_NAME: &str = "unnamed-job";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WorkflowTaskKind {
    Job,
    Step,
}

pub(super) trait WorkflowErrorReporter: fmt::Debug + Send + Sync {
    fn report(&self, error: SyntheticWorkflowError);
}

pub(super) struct SentryWorkflowErrorReporter {
    client: Arc<Client>,
}

impl SentryWorkflowErrorReporter {
    pub(super) fn new(client: Arc<Client>) -> Self {
        Self { client }
    }
}

impl WorkflowErrorReporter for SentryWorkflowErrorReporter {
    fn report(&self, error: SyntheticWorkflowError) {
        let mut contexts = sentry::protocol::Map::new();
        contexts.insert(
            "trace".to_owned(),
            Context::Trace(Box::new(TraceContext {
                span_id: error.span_id().to_bytes().into(),
                trace_id: error.trace_id().to_bytes().into(),
                op: Some(error.kind().span_operation().to_owned()),
                description: Some(error.task_name().to_owned()),
                status: Some(SpanStatus::InternalError),
                origin: Some("manual.github.workflow".to_owned()),
                ..TraceContext::default()
            })),
        );
        let fingerprint = error
            .fingerprint()
            .into_iter()
            .map(|value| Cow::Owned(value.to_owned()))
            .collect();
        let tags = [
            ("github.repository.name", error.repository_name()),
            ("cicd.pipeline.name", error.workflow_name()),
            ("cicd.pipeline.task.name", error.task_name()),
            ("cicd.pipeline.task.run.id", error.task_run_id()),
            ("github.workflow.conclusion", error.conclusion()),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect();
        let exception = Exception {
            ty: error.exception_type().to_owned(),
            value: Some(error.description().to_owned()),
            mechanism: Some(Mechanism {
                ty: "github_actions".to_owned(),
                handled: Some(true),
                synthetic: Some(true),
                ..Mechanism::default()
            }),
            ..Exception::default()
        };
        self.client.capture_event(
            Event {
                level: Level::Error,
                fingerprint,
                timestamp: error.timestamp(),
                contexts,
                exception: vec![exception].into(),
                tags,
                ..Event::default()
            },
            None,
        );
    }
}

impl fmt::Debug for SentryWorkflowErrorReporter {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SentryWorkflowErrorReporter([REDACTED])")
    }
}

struct WorkflowErrorParts {
    kind: WorkflowTaskKind,
    conclusion: WorkflowConclusion,
    task_name: String,
    grouping_task_name: String,
    task_run_id: String,
    timestamp: SystemTime,
    trace_id: TraceId,
    span_id: SpanId,
}

pub(super) struct SyntheticWorkflowError {
    kind: WorkflowTaskKind,
    exception_type: &'static str,
    description: String,
    repository_name: String,
    workflow_name: String,
    task_name: String,
    grouping_job_name: String,
    grouping_task_name: String,
    task_run_id: String,
    conclusion: &'static str,
    timestamp: SystemTime,
    trace_id: TraceId,
    span_id: SpanId,
}

impl SyntheticWorkflowError {
    pub(super) fn for_step(
        job: &WorkflowJobTrace,
        step: &WorkflowStepTrace,
        trace_id: TraceId,
        span_id: SpanId,
    ) -> Self {
        let task_run_id = format!("{}:{}", job.job_id().get(), step.number());
        let task_name = step.name().map_or_else(
            || format!("task {task_run_id}"),
            |name| name.as_str().to_owned(),
        );
        let grouping_task_name = step.name().map_or_else(
            || format!("unnamed-step:{}", step.number()),
            |name| name.as_str().to_owned(),
        );
        Self::new(
            job,
            WorkflowErrorParts {
                kind: WorkflowTaskKind::Step,
                conclusion: step.conclusion(),
                task_name,
                grouping_task_name,
                task_run_id,
                timestamp: step.timing().end(),
                trace_id,
                span_id,
            },
        )
    }

    pub(super) fn for_job(job: &WorkflowJobTrace, trace_id: TraceId, span_id: SpanId) -> Self {
        let task_run_id = job.job_id().get().to_string();
        let task_name = job.job_name().map_or_else(
            || format!("task {task_run_id}"),
            |name| name.as_str().to_owned(),
        );
        let grouping_task_name = job.job_name().map_or_else(
            || UNNAMED_JOB_GROUPING_NAME.to_owned(),
            |name| name.as_str().to_owned(),
        );
        Self::new(
            job,
            WorkflowErrorParts {
                kind: WorkflowTaskKind::Job,
                conclusion: job.conclusion(),
                task_name,
                grouping_task_name,
                task_run_id,
                timestamp: job.timing().end(),
                trace_id,
                span_id,
            },
        )
    }

    fn new(job: &WorkflowJobTrace, parts: WorkflowErrorParts) -> Self {
        let (exception_type, description_prefix) = match parts.conclusion {
            WorkflowConclusion::Failure => ("GitHubActionsTaskFailure", "CI task failed: "),
            WorkflowConclusion::TimedOut => ("GitHubActionsTaskTimeout", "CI task timed out: "),
            WorkflowConclusion::Success
            | WorkflowConclusion::Cancelled
            | WorkflowConclusion::Skipped
            | WorkflowConclusion::Neutral
            | WorkflowConclusion::Other => {
                unreachable!("synthetic errors require a failing workflow conclusion")
            }
        };
        let mut description =
            String::with_capacity(description_prefix.len() + parts.task_name.len());
        description.push_str(description_prefix);
        description.push_str(&parts.task_name);
        Self {
            kind: parts.kind,
            exception_type,
            description,
            repository_name: job.repository_name().as_str().to_owned(),
            workflow_name: job
                .workflow_name()
                .map_or(UNKNOWN_WORKFLOW_NAME.to_owned(), |name| {
                    name.as_str().to_owned()
                }),
            task_name: parts.task_name,
            grouping_job_name: job.job_name().map_or_else(
                || UNNAMED_JOB_GROUPING_NAME.to_owned(),
                |name| name.as_str().to_owned(),
            ),
            grouping_task_name: parts.grouping_task_name,
            task_run_id: parts.task_run_id,
            conclusion: parts.conclusion.as_str(),
            timestamp: parts.timestamp,
            trace_id: parts.trace_id,
            span_id: parts.span_id,
        }
    }

    pub(super) const fn kind(&self) -> WorkflowTaskKind {
        self.kind
    }

    pub(super) const fn exception_type(&self) -> &'static str {
        self.exception_type
    }

    pub(super) fn description(&self) -> &str {
        &self.description
    }

    pub(super) fn repository_name(&self) -> &str {
        &self.repository_name
    }

    pub(super) fn workflow_name(&self) -> &str {
        &self.workflow_name
    }

    pub(super) fn task_name(&self) -> &str {
        &self.task_name
    }

    pub(super) fn task_run_id(&self) -> &str {
        &self.task_run_id
    }

    pub(super) const fn conclusion(&self) -> &'static str {
        self.conclusion
    }

    pub(super) fn span_event_attributes(&self) -> [KeyValue; 2] {
        [
            KeyValue::new("exception.type", self.exception_type),
            KeyValue::new("exception.message", self.description.clone()),
        ]
    }

    pub(super) fn timestamp(&self) -> SystemTime {
        self.timestamp
    }

    pub(super) const fn trace_id(&self) -> TraceId {
        self.trace_id
    }

    pub(super) const fn span_id(&self) -> SpanId {
        self.span_id
    }

    pub(super) fn fingerprint(&self) -> [&str; 7] {
        [
            "github-actions-task",
            self.kind.as_str(),
            &self.repository_name,
            &self.workflow_name,
            &self.grouping_job_name,
            &self.grouping_task_name,
            self.conclusion,
        ]
    }
}

impl WorkflowTaskKind {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Job => "job",
            Self::Step => "step",
        }
    }

    const fn span_operation(self) -> &'static str {
        match self {
            Self::Job => "github.workflow.job",
            Self::Step => "github.workflow.step",
        }
    }
}

impl fmt::Debug for SyntheticWorkflowError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SyntheticWorkflowError([REDACTED])")
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::Arc,
        time::{Duration, SystemTime},
    };

    use opentelemetry::trace::{SpanId, TraceId};
    use sentry::{protocol::Context as SentryContext, test::TestTransport, Client, ClientOptions};

    use super::{
        SentryWorkflowErrorReporter, SyntheticWorkflowError, WorkflowErrorReporter,
        WorkflowTaskKind,
    };
    use crate::{
        domain::{delivery::DeliveryId, merge_queue::PullRequestNumber},
        security::CanonicalRepositoryName,
        telemetry::{
            trace::CommitSha,
            workflow::{
                DisplayName, HistoricalTiming, WorkflowConclusion, WorkflowJobId, WorkflowJobTrace,
                WorkflowJobTraceParts, WorkflowPullRequests, WorkflowRunAttempt, WorkflowRunId,
                WorkflowStepTrace,
            },
        },
    };

    fn failure_job(
        job_name: Option<&str>,
        step_name: Option<&str>,
        conclusion: WorkflowConclusion,
    ) -> WorkflowJobTrace {
        failure_job_with_id(41, job_name, step_name, conclusion)
    }

    fn failure_job_with_id(
        job_id: i64,
        job_name: Option<&str>,
        step_name: Option<&str>,
        conclusion: WorkflowConclusion,
    ) -> WorkflowJobTrace {
        let timing = HistoricalTiming::fallback(SystemTime::UNIX_EPOCH + Duration::from_secs(20));
        let step = WorkflowStepTrace::new(
            2,
            step_name.and_then(DisplayName::sanitize),
            conclusion,
            timing.clone(),
        )
        .expect("step is valid");
        WorkflowJobTrace::new(WorkflowJobTraceParts {
            repository_name: CanonicalRepositoryName::new("Owner/Repository")
                .expect("repository is valid"),
            delivery_id: DeliveryId::parse("550e8400-e29b-41d4-a716-446655440000")
                .expect("delivery id is valid"),
            workflow_name: DisplayName::sanitize("Build Workflow"),
            run_id: WorkflowRunId::new(31).expect("run id is valid"),
            run_attempt: WorkflowRunAttempt::new(2).expect("run attempt is valid"),
            job_id: WorkflowJobId::new(job_id).expect("job id is valid"),
            job_name: job_name.and_then(DisplayName::sanitize),
            conclusion,
            head_sha: Some(
                CommitSha::parse("0123456789abcdef0123456789abcdef01234567")
                    .expect("commit SHA is valid"),
            ),
            pull_requests: WorkflowPullRequests::new([
                PullRequestNumber::new(7).expect("pull request number is valid")
            ]),
            workflow_run_context: None,
            timing,
            steps: vec![step],
        })
    }

    #[test]
    fn step_errors_include_the_sanitized_task_name_and_stable_grouping() {
        let job = failure_job(
            Some("Linux Job"),
            Some("cargo test"),
            WorkflowConclusion::Failure,
        );
        let error = SyntheticWorkflowError::for_step(
            &job,
            &job.steps()[0],
            TraceId::from_bytes([1; 16]),
            SpanId::from_bytes([2; 8]),
        );

        assert_eq!(error.kind(), WorkflowTaskKind::Step);
        assert_eq!(error.exception_type(), "GitHubActionsTaskFailure");
        assert_eq!(error.description(), "CI task failed: cargo test");
        assert_eq!(error.task_name(), "cargo test");
        assert_eq!(error.task_run_id(), "41:2");
        assert_eq!(
            error.fingerprint(),
            [
                "github-actions-task",
                "step",
                "owner/repository",
                "Build Workflow",
                "Linux Job",
                "cargo test",
                "failure",
            ]
        );
    }

    #[test]
    fn unnamed_task_grouping_is_stable_across_job_ids() {
        let first_job = failure_job_with_id(41, None, None, WorkflowConclusion::Failure);
        let second_job = failure_job_with_id(99, None, None, WorkflowConclusion::Failure);
        let first_step_error = SyntheticWorkflowError::for_step(
            &first_job,
            &first_job.steps()[0],
            TraceId::from_bytes([1; 16]),
            SpanId::from_bytes([2; 8]),
        );
        let second_step_error = SyntheticWorkflowError::for_step(
            &second_job,
            &second_job.steps()[0],
            TraceId::from_bytes([3; 16]),
            SpanId::from_bytes([4; 8]),
        );
        let first_job_error = SyntheticWorkflowError::for_job(
            &first_job,
            TraceId::from_bytes([1; 16]),
            SpanId::from_bytes([5; 8]),
        );
        let second_job_error = SyntheticWorkflowError::for_job(
            &second_job,
            TraceId::from_bytes([3; 16]),
            SpanId::from_bytes([6; 8]),
        );

        assert_ne!(first_step_error.task_name(), second_step_error.task_name());
        assert_ne!(first_job_error.task_name(), second_job_error.task_name());
        assert_eq!(
            first_step_error.fingerprint(),
            second_step_error.fingerprint()
        );
        assert_eq!(
            first_step_error.fingerprint(),
            [
                "github-actions-task",
                "step",
                "owner/repository",
                "Build Workflow",
                "unnamed-job",
                "unnamed-step:2",
                "failure",
            ]
        );
        assert_eq!(
            first_job_error.fingerprint(),
            second_job_error.fingerprint()
        );
        assert_eq!(
            first_job_error.fingerprint(),
            [
                "github-actions-task",
                "job",
                "owner/repository",
                "Build Workflow",
                "unnamed-job",
                "unnamed-job",
                "failure",
            ]
        );

        let first_named_step_job =
            failure_job_with_id(41, None, Some("cargo test"), WorkflowConclusion::Failure);
        let second_named_step_job =
            failure_job_with_id(99, None, Some("cargo test"), WorkflowConclusion::Failure);
        let first_named_step_error = SyntheticWorkflowError::for_step(
            &first_named_step_job,
            &first_named_step_job.steps()[0],
            TraceId::from_bytes([7; 16]),
            SpanId::from_bytes([8; 8]),
        );
        let second_named_step_error = SyntheticWorkflowError::for_step(
            &second_named_step_job,
            &second_named_step_job.steps()[0],
            TraceId::from_bytes([9; 16]),
            SpanId::from_bytes([10; 8]),
        );
        assert_eq!(
            first_named_step_error.fingerprint(),
            second_named_step_error.fingerprint()
        );
    }

    #[test]
    fn job_and_step_grouping_are_distinct_for_equal_names() {
        let job = failure_job(
            Some("shared task"),
            Some("shared task"),
            WorkflowConclusion::Failure,
        );
        let step_error = SyntheticWorkflowError::for_step(
            &job,
            &job.steps()[0],
            TraceId::from_bytes([1; 16]),
            SpanId::from_bytes([2; 8]),
        );
        let job_error = SyntheticWorkflowError::for_job(
            &job,
            TraceId::from_bytes([1; 16]),
            SpanId::from_bytes([3; 8]),
        );

        assert_ne!(step_error.fingerprint(), job_error.fingerprint());
    }

    #[test]
    fn sentry_reporter_emits_a_linked_synthetic_exception_event() {
        let transport = TestTransport::new();
        let options = ClientOptions::new()
            .dsn("https://public@sentry.invalid/1")
            .transport(transport.clone());
        let reporter = SentryWorkflowErrorReporter::new(Arc::new(Client::from_config(options)));
        let job = failure_job(
            Some("Linux Job"),
            Some("cargo test"),
            WorkflowConclusion::Failure,
        );
        let error = SyntheticWorkflowError::for_step(
            &job,
            &job.steps()[0],
            TraceId::from_bytes([1; 16]),
            SpanId::from_bytes([2; 8]),
        );

        reporter.report(error);

        let events = transport.fetch_and_clear_events();
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(
            event
                .fingerprint
                .iter()
                .map(AsRef::as_ref)
                .collect::<Vec<_>>(),
            [
                "github-actions-task",
                "step",
                "owner/repository",
                "Build Workflow",
                "Linux Job",
                "cargo test",
                "failure",
            ]
        );
        assert_eq!(event.exception.values.len(), 1);
        let exception = &event.exception.values[0];
        assert_eq!(exception.ty, "GitHubActionsTaskFailure");
        assert_eq!(
            exception.value.as_deref(),
            Some("CI task failed: cargo test")
        );
        let mechanism = exception.mechanism.as_ref().expect("mechanism is present");
        assert_eq!(mechanism.ty, "github_actions");
        assert_eq!(mechanism.handled, Some(true));
        assert_eq!(mechanism.synthetic, Some(true));
        match event.contexts.get("trace") {
            Some(SentryContext::Trace(trace)) => {
                assert_eq!(
                    trace.trace_id.to_string(),
                    TraceId::from_bytes([1; 16]).to_string()
                );
                assert_eq!(
                    trace.span_id.to_string(),
                    SpanId::from_bytes([2; 8]).to_string()
                );
            }
            _ => panic!("trace context is present"),
        }
        assert_eq!(
            event.tags.get("github.repository.name").map(String::as_str),
            Some("owner/repository")
        );
        assert_eq!(
            event
                .tags
                .get("cicd.pipeline.task.name")
                .map(String::as_str),
            Some("cargo test")
        );
        assert_eq!(
            event
                .tags
                .get("github.workflow.conclusion")
                .map(String::as_str),
            Some("failure")
        );
    }

    #[test]
    fn missing_names_use_validated_task_run_identifiers_without_raw_data() {
        let job = failure_job(None, None, WorkflowConclusion::TimedOut);
        let step_error = SyntheticWorkflowError::for_step(
            &job,
            &job.steps()[0],
            TraceId::from_bytes([1; 16]),
            SpanId::from_bytes([2; 8]),
        );
        let job_error = SyntheticWorkflowError::for_job(
            &job,
            TraceId::from_bytes([1; 16]),
            SpanId::from_bytes([3; 8]),
        );

        assert_eq!(step_error.exception_type(), "GitHubActionsTaskTimeout");
        assert_eq!(step_error.description(), "CI task timed out: task 41:2");
        assert_eq!(step_error.task_name(), "task 41:2");
        assert_eq!(job_error.kind(), WorkflowTaskKind::Job);
        assert_eq!(job_error.description(), "CI task timed out: task 41");
        assert_eq!(job_error.task_name(), "task 41");
    }
}
