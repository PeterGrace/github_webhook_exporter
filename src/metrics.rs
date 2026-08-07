use std::{
    fmt::{self, Write as _},
    sync::{atomic::AtomicU64, Arc},
    time::Duration,
};

use axum::{
    body::Body,
    extract::State,
    http::{header::CONTENT_TYPE, StatusCode},
    response::Response,
    routing::get,
    Router,
};
use prometheus_client::{
    encoding::{text::encode, EncodeLabelSet, EncodeLabelValue, LabelValueEncoder},
    metrics::{counter::Counter, family::Family, gauge::Gauge, histogram::Histogram},
    registry::Registry,
};

use crate::{app::AppState, error::AppError};

const OPEN_METRICS_CONTENT_TYPE: &str =
    "application/openmetrics-text; version=1.0.0; charset=utf-8";
const REQUEST_DURATION_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];
const REQUEST_BODY_SIZE_BUCKETS: [f64; 10] = [
    128.0,
    512.0,
    1_024.0,
    4_096.0,
    16_384.0,
    65_536.0,
    262_144.0,
    524_288.0,
    1_048_576.0,
    2_097_152.0,
];
const MERGE_QUEUE_ATTEMPT_DURATION_BUCKETS: [f64; 15] = [
    1.0,
    10.0,
    60.0,
    300.0,
    900.0,
    3_600.0,
    10_800.0,
    21_600.0,
    43_200.0,
    86_400.0,
    259_200.0,
    604_800.0,
    2_592_000.0,
    7_776_000.0,
    31_536_000.0,
];
const WORKFLOW_JOB_STEP_BUCKETS: [f64; 10] = [
    0.0, 5.0, 10.0, 20.0, 40.0, 64.0, 128.0, 256.0, 512.0, 1_024.0,
];

/// Largest merge-queue attempt duration accepted by the metrics sanity check.
pub const MAX_MERGE_QUEUE_ATTEMPT_DURATION: time::Duration = time::Duration::days(365);

/// A normalized GitHub webhook event type from the fixed v1 vocabulary.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum EventType {
    /// A branch protection rule event.
    BranchProtectionRule,
    /// A check run event.
    CheckRun,
    /// A check suite event.
    CheckSuite,
    /// A Git reference creation event.
    Create,
    /// A Git reference deletion event.
    Delete,
    /// A deployment event.
    Deployment,
    /// A deployment status event.
    DeploymentStatus,
    /// A discussion event.
    Discussion,
    /// A discussion comment event.
    DiscussionComment,
    /// An issues event.
    Issues,
    /// An issue comment event.
    IssueComment,
    /// A merge group event.
    MergeGroup,
    /// A pull request event.
    PullRequest,
    /// A pull request review event.
    PullRequestReview,
    /// A pull request review comment event.
    PullRequestReviewComment,
    /// A push event.
    Push,
    /// A release event.
    Release,
    /// A repository event.
    Repository,
    /// A commit status event.
    Status,
    /// A workflow job event.
    WorkflowJob,
    /// A workflow run event.
    WorkflowRun,
    /// Any event outside the fixed v1 vocabulary.
    Other,
}

impl EventType {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::BranchProtectionRule => "branch_protection_rule",
            Self::CheckRun => "check_run",
            Self::CheckSuite => "check_suite",
            Self::Create => "create",
            Self::Delete => "delete",
            Self::Deployment => "deployment",
            Self::DeploymentStatus => "deployment_status",
            Self::Discussion => "discussion",
            Self::DiscussionComment => "discussion_comment",
            Self::Issues => "issues",
            Self::IssueComment => "issue_comment",
            Self::MergeGroup => "merge_group",
            Self::PullRequest => "pull_request",
            Self::PullRequestReview => "pull_request_review",
            Self::PullRequestReviewComment => "pull_request_review_comment",
            Self::Push => "push",
            Self::Release => "release",
            Self::Repository => "repository",
            Self::Status => "status",
            Self::WorkflowJob => "workflow_job",
            Self::WorkflowRun => "workflow_run",
            Self::Other => "other",
        }
    }
}

impl EncodeLabelValue for EventType {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        encoder.write_str(self.as_str())
    }
}

/// A normalized GitHub webhook action from the fixed global v1 vocabulary.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Action {
    /// The `assigned` action.
    Assigned,
    /// The `checks_requested` action.
    ChecksRequested,
    /// The `closed` action.
    Closed,
    /// The `completed` action.
    Completed,
    /// The `created` action.
    Created,
    /// The `deleted` action.
    Deleted,
    /// The `dequeued` action.
    Dequeued,
    /// The `destroyed` action.
    Destroyed,
    /// The `edited` action.
    Edited,
    /// The `enqueued` action.
    Enqueued,
    /// The `in_progress` action.
    InProgress,
    /// The `labeled` action.
    Labeled,
    /// The `opened` action.
    Opened,
    /// The `published` action.
    Published,
    /// The `queued` action.
    Queued,
    /// The `reopened` action.
    Reopened,
    /// The `requested` action.
    Requested,
    /// The `requested_action` action.
    RequestedAction,
    /// The `rerequested` action.
    Rerequested,
    /// The `submitted` action.
    Submitted,
    /// The `synchronize` action.
    Synchronize,
    /// The `unassigned` action.
    Unassigned,
    /// The `unlabeled` action.
    Unlabeled,
    /// The `unpublished` action.
    Unpublished,
    /// The `updated` action.
    Updated,
    /// The `waiting` action.
    Waiting,
    /// An event payload with no action field.
    None,
    /// Any present action outside the fixed v1 vocabulary.
    Other,
}

impl Action {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Assigned => "assigned",
            Self::ChecksRequested => "checks_requested",
            Self::Closed => "closed",
            Self::Completed => "completed",
            Self::Created => "created",
            Self::Deleted => "deleted",
            Self::Dequeued => "dequeued",
            Self::Destroyed => "destroyed",
            Self::Edited => "edited",
            Self::Enqueued => "enqueued",
            Self::InProgress => "in_progress",
            Self::Labeled => "labeled",
            Self::Opened => "opened",
            Self::Published => "published",
            Self::Queued => "queued",
            Self::Reopened => "reopened",
            Self::Requested => "requested",
            Self::RequestedAction => "requested_action",
            Self::Rerequested => "rerequested",
            Self::Submitted => "submitted",
            Self::Synchronize => "synchronize",
            Self::Unassigned => "unassigned",
            Self::Unlabeled => "unlabeled",
            Self::Unpublished => "unpublished",
            Self::Updated => "updated",
            Self::Waiting => "waiting",
            Self::None => "none",
            Self::Other => "other",
        }
    }
}

impl EncodeLabelValue for Action {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        encoder.write_str(self.as_str())
    }
}

/// A bounded result recorded for every webhook request.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum WebhookResult {
    /// The request was accepted.
    Accepted,
    /// The request was malformed.
    Malformed,
    /// The request failed authentication or repository authorization.
    Unauthorized,
    /// The request body exceeded the configured limit.
    TooLarge,
    /// The request media type was unsupported.
    Unsupported,
    /// A required internal dependency was unavailable.
    Unavailable,
}

/// A bounded stage at which webhook processing can fail.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum FailureStage {
    /// Authentication or secret-loading failure.
    Authentication,
    /// Durable delivery-claim failure.
    DeliveryClaim,
    /// Metrics update or exposition failure.
    Metrics,
    /// General database failure.
    Database,
    /// Durable merge-queue state transition failure.
    QueueState,
}

/// A bounded merge-group webhook action.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum MergeGroupAction {
    /// GitHub requested checks for a newly created merge group.
    ChecksRequested,
    /// GitHub destroyed an existing merge group.
    Destroyed,
}

impl MergeGroupAction {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ChecksRequested => "checks_requested",
            Self::Destroyed => "destroyed",
        }
    }
}

impl EncodeLabelValue for MergeGroupAction {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        encoder.write_str(self.as_str())
    }
}

/// A bounded reason associated with a merge-group action.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum MergeGroupReason {
    /// No reason applies to a `checks_requested` action.
    None,
    /// GitHub destroyed the group after it merged.
    Merged,
    /// GitHub destroyed the group after it left the queue.
    Dequeued,
    /// GitHub invalidated the group.
    Invalidated,
    /// The raw reason was absent or outside the fixed vocabulary.
    Other,
}

impl MergeGroupReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Merged => "merged",
            Self::Dequeued => "dequeued",
            Self::Invalidated => "invalidated",
            Self::Other => "other",
        }
    }
}

impl EncodeLabelValue for MergeGroupReason {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        encoder.write_str(self.as_str())
    }
}

/// A bounded terminal merge-queue attempt outcome.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum MergeQueueOutcome {
    /// The pull request merged successfully.
    Succeeded,
    /// Reserved for a future evidence-backed failure classifier.
    Failed,
    /// Reserved for a future evidence-backed cancellation classifier.
    Cancelled,
    /// The attempt ended without a supported semantic classification.
    Unknown,
}

impl MergeQueueOutcome {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }
}

impl EncodeLabelValue for MergeQueueOutcome {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        encoder.write_str(self.as_str())
    }
}

/// A bounded terminal reason for one pull request's queue attempt.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum MergeQueueReason {
    /// A merged pull-request event proved success.
    PullRequestMerged,
    /// A dequeue ended the attempt without evidence for a stronger classification.
    UnclassifiedDequeue,
}

impl MergeQueueReason {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::PullRequestMerged => "pull_request_merged",
            Self::UnclassifiedDequeue => "unclassified_dequeue",
        }
    }
}

impl EncodeLabelValue for MergeQueueReason {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        encoder.write_str(self.as_str())
    }
}

/// An evidence-backed terminal queue completion with an invariant outcome/reason pairing.
///
/// Failed and cancelled outcomes remain reserved until a future classifier revision adds explicit
/// evidence-backed completion variants and reason vocabulary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeQueueCompletion {
    /// A merged pull-request event proves `succeeded` with `pull_request_merged`.
    PullRequestMerged,
    /// A dequeue records `unknown` with `unclassified_dequeue`.
    UnclassifiedDequeue,
}

impl MergeQueueCompletion {
    fn labels(self) -> MergeQueueOutcomeLabels {
        match self {
            Self::PullRequestMerged => MergeQueueOutcomeLabels {
                outcome: MergeQueueOutcome::Succeeded,
                reason: MergeQueueReason::PullRequestMerged,
            },
            Self::UnclassifiedDequeue => MergeQueueOutcomeLabels {
                outcome: MergeQueueOutcome::Unknown,
                reason: MergeQueueReason::UnclassifiedDequeue,
            },
        }
    }
}

/// A bounded reason for a rejected or impossible merge-queue transition.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum QueueTransitionFailureReason {
    /// A completion event had no active durable attempt.
    MissingActiveAttempt,
    /// A computed attempt duration was negative or exceeded the sanity ceiling.
    InvalidDuration,
}

/// A bounded reason for rejecting a workflow-job trace.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum WorkflowTraceRejectionReason {
    /// The trace reported more steps than the configured maximum.
    TooManySteps,
}

impl WorkflowTraceRejectionReason {
    /// Returns the fixed metric/log vocabulary value for this rejection reason.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::TooManySteps => "too_many_steps",
        }
    }
}

impl EncodeLabelValue for WorkflowTraceRejectionReason {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        encoder.write_str(self.as_str())
    }
}

impl QueueTransitionFailureReason {
    fn as_str(self) -> &'static str {
        match self {
            Self::MissingActiveAttempt => "missing_active_attempt",
            Self::InvalidDuration => "invalid_duration",
        }
    }
}

impl EncodeLabelValue for QueueTransitionFailureReason {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        encoder.write_str(self.as_str())
    }
}

impl WebhookResult {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Malformed => "malformed",
            Self::Unauthorized => "unauthorized",
            Self::TooLarge => "too_large",
            Self::Unsupported => "unsupported",
            Self::Unavailable => "unavailable",
        }
    }
}

impl EncodeLabelValue for WebhookResult {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        encoder.write_str(self.as_str())
    }
}

impl FailureStage {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Authentication => "authentication",
            Self::DeliveryClaim => "delivery_claim",
            Self::Metrics => "metrics",
            Self::Database => "database",
            Self::QueueState => "queue_state",
        }
    }
}

impl EncodeLabelValue for FailureStage {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        encoder.write_str(self.as_str())
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct RequestLabels {
    result: WebhookResult,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct EventLabels {
    event_type: EventType,
    action: Action,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct FailureLabels {
    stage: FailureStage,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct MergeGroupLabels {
    action: MergeGroupAction,
    reason: MergeGroupReason,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct MergeQueueOutcomeLabels {
    outcome: MergeQueueOutcome,
    reason: MergeQueueReason,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct MergeQueueDurationLabels {
    outcome: MergeQueueOutcome,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct QueueTransitionFailureLabels {
    reason: QueueTransitionFailureReason,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
struct WorkflowTraceRejectionLabels {
    reason: WorkflowTraceRejectionReason,
}

type CounterFamily<L> = Family<L, Counter>;
type HistogramFamily<L> = Family<L, Histogram, fn() -> Histogram>;
type RepositoryGauge = Gauge<u64, AtomicU64>;

/// Cloneable, thread-safe Prometheus instruments for webhook processing.
///
/// The registry and instruments remain private so callers can update metrics only through methods
/// whose arguments enforce the fixed label vocabularies.
#[derive(Clone, Debug)]
pub struct Metrics {
    inner: Arc<MetricsInner>,
}

#[derive(Debug)]
struct MetricsInner {
    registry: Registry,
    webhook_requests: CounterFamily<RequestLabels>,
    webhook_events: CounterFamily<EventLabels>,
    processing_duration: HistogramFamily<RequestLabels>,
    request_body_bytes: Histogram,
    duplicates: Counter,
    processing_failures: CounterFamily<FailureLabels>,
    repository_configurations: RepositoryGauge,
    merge_group_events: CounterFamily<MergeGroupLabels>,
    merge_queue_pr_outcomes: CounterFamily<MergeQueueOutcomeLabels>,
    merge_queue_attempt_duration: HistogramFamily<MergeQueueDurationLabels>,
    merge_queue_transition_failures: CounterFamily<QueueTransitionFailureLabels>,
    workflow_job_steps: Histogram,
    workflow_trace_rejections: CounterFamily<WorkflowTraceRejectionLabels>,
}

impl Metrics {
    /// Creates an empty metrics registry containing every Phase 2 and Phase 3 instrument.
    pub fn new() -> Self {
        let webhook_requests = CounterFamily::default();
        let webhook_events = CounterFamily::default();
        let processing_duration =
            HistogramFamily::new_with_constructor(request_duration_histogram as fn() -> Histogram);
        let request_body_bytes = request_body_size_histogram();
        let duplicates = Counter::default();
        let processing_failures = CounterFamily::default();
        let repository_configurations = RepositoryGauge::default();
        let merge_group_events = CounterFamily::default();
        let merge_queue_pr_outcomes = CounterFamily::default();
        let merge_queue_attempt_duration = HistogramFamily::new_with_constructor(
            merge_queue_attempt_duration_histogram as fn() -> Histogram,
        );
        let merge_queue_transition_failures = CounterFamily::default();
        let workflow_job_steps = Histogram::new(WORKFLOW_JOB_STEP_BUCKETS);
        let workflow_trace_rejections = CounterFamily::default();
        let mut registry = Registry::with_prefix("github");
        let _ = Self::observe_workflow_job_steps as fn(&Self, usize);
        let _ = Self::record_workflow_trace_rejection as fn(&Self, WorkflowTraceRejectionReason);

        // `prometheus-client` omits a labelled family until it owns at least one metric. Seed only
        // bounded zero-valued labels so every required family exists before the first webhook.
        for result in [
            WebhookResult::Accepted,
            WebhookResult::Malformed,
            WebhookResult::Unauthorized,
            WebhookResult::TooLarge,
            WebhookResult::Unsupported,
            WebhookResult::Unavailable,
        ] {
            let labels = RequestLabels { result };
            let _ = webhook_requests.get_or_create(&labels);
            let _ = processing_duration.get_or_create(&labels);
        }
        let _ = webhook_events.get_or_create(&EventLabels {
            event_type: EventType::Other,
            action: Action::Other,
        });
        for stage in [
            FailureStage::Authentication,
            FailureStage::DeliveryClaim,
            FailureStage::Metrics,
            FailureStage::Database,
            FailureStage::QueueState,
        ] {
            let _ = processing_failures.get_or_create(&FailureLabels { stage });
        }
        let _ = merge_group_events.get_or_create(&MergeGroupLabels {
            action: MergeGroupAction::ChecksRequested,
            reason: MergeGroupReason::None,
        });
        for reason in [
            MergeGroupReason::Merged,
            MergeGroupReason::Dequeued,
            MergeGroupReason::Invalidated,
            MergeGroupReason::Other,
        ] {
            let _ = merge_group_events.get_or_create(&MergeGroupLabels {
                action: MergeGroupAction::Destroyed,
                reason,
            });
        }
        for (outcome, reason) in [
            (
                MergeQueueOutcome::Succeeded,
                MergeQueueReason::PullRequestMerged,
            ),
            (
                MergeQueueOutcome::Unknown,
                MergeQueueReason::UnclassifiedDequeue,
            ),
        ] {
            let _ =
                merge_queue_pr_outcomes.get_or_create(&MergeQueueOutcomeLabels { outcome, reason });
            let _ =
                merge_queue_attempt_duration.get_or_create(&MergeQueueDurationLabels { outcome });
        }
        for reason in [
            QueueTransitionFailureReason::MissingActiveAttempt,
            QueueTransitionFailureReason::InvalidDuration,
        ] {
            let _ = merge_queue_transition_failures
                .get_or_create(&QueueTransitionFailureLabels { reason });
        }
        let _ = workflow_trace_rejections.get_or_create(&WorkflowTraceRejectionLabels {
            reason: WorkflowTraceRejectionReason::TooManySteps,
        });

        registry.register(
            "webhook_requests",
            "Total webhook requests by bounded result",
            webhook_requests.clone(),
        );
        registry.register(
            "webhook_events",
            "Authenticated newly claimed webhook events by bounded event and action",
            webhook_events.clone(),
        );
        registry.register(
            "webhook_processing_duration_seconds",
            "Webhook processing duration in seconds by bounded result",
            processing_duration.clone(),
        );
        registry.register(
            "webhook_request_body_bytes",
            "Authenticated newly claimed webhook request body size in bytes",
            request_body_bytes.clone(),
        );
        registry.register(
            "webhook_duplicates",
            "Total authenticated duplicate webhook deliveries",
            duplicates.clone(),
        );
        registry.register(
            "webhook_processing_failures",
            "Total webhook processing failures by bounded stage",
            processing_failures.clone(),
        );
        registry.register(
            "repository_configurations",
            "Current configured repository record count",
            repository_configurations.clone(),
        );
        registry.register(
            "merge_group_events",
            "Merge-group webhook events by bounded action and reason",
            merge_group_events.clone(),
        );
        registry.register(
            "merge_queue_pr_outcomes",
            "Completed pull-request queue attempts by bounded outcome and reason",
            merge_queue_pr_outcomes.clone(),
        );
        registry.register(
            "merge_queue_attempt_duration_seconds",
            "Valid completed merge-queue attempt duration in seconds by bounded outcome",
            merge_queue_attempt_duration.clone(),
        );
        registry.register(
            "merge_queue_transition_failures",
            "Merge-queue transition failures by bounded reason",
            merge_queue_transition_failures.clone(),
        );
        registry.register(
            "workflow_job_steps",
            "Reported step count for structurally valid newly claimed completed workflow jobs",
            workflow_job_steps.clone(),
        );
        registry.register(
            "workflow_job_trace_rejections",
            "Completed workflow-job traces rejected by bounded reason",
            workflow_trace_rejections.clone(),
        );

        Self {
            inner: Arc::new(MetricsInner {
                registry,
                webhook_requests,
                webhook_events,
                processing_duration,
                request_body_bytes,
                duplicates,
                processing_failures,
                repository_configurations,
                merge_group_events,
                merge_queue_pr_outcomes,
                merge_queue_attempt_duration,
                merge_queue_transition_failures,
                workflow_job_steps,
                workflow_trace_rejections,
            }),
        }
    }

    /// Records one webhook request and its end-to-end processing duration.
    ///
    /// `result` is a closed enum and `duration` is observed in seconds.
    pub fn observe_request(&self, result: WebhookResult, duration: Duration) {
        let labels = RequestLabels { result };
        self.inner.webhook_requests.get_or_create(&labels).inc();
        self.inner
            .processing_duration
            .get_or_create(&labels)
            .observe(duration.as_secs_f64());
    }

    /// Records one authenticated, newly claimed event and its request-body size.
    pub fn observe_event(&self, event_type: EventType, action: Action, body_bytes: usize) {
        self.inner
            .webhook_events
            .get_or_create(&EventLabels { event_type, action })
            .inc();
        self.inner.request_body_bytes.observe(body_bytes as f64);
    }

    /// Increments the authenticated duplicate-delivery total.
    pub fn record_duplicate(&self) {
        self.inner.duplicates.inc();
    }

    /// Increments the processing-failure total for a bounded stage.
    pub fn record_failure(&self, stage: FailureStage) {
        self.inner
            .processing_failures
            .get_or_create(&FailureLabels { stage })
            .inc();
    }

    /// Records one merge-group event using only bounded action and reason labels.
    ///
    /// `checks_requested` always records reason `none`, regardless of the supplied reason. This
    /// prevents callers from creating unsupported action/reason combinations.
    pub fn record_merge_group_event(&self, action: MergeGroupAction, reason: MergeGroupReason) {
        let reason = match action {
            MergeGroupAction::ChecksRequested => MergeGroupReason::None,
            MergeGroupAction::Destroyed => reason,
        };
        self.inner
            .merge_group_events
            .get_or_create(&MergeGroupLabels { action, reason })
            .inc();
    }

    /// Records one completed pull-request queue attempt when its duration is sane.
    ///
    /// Negative durations and durations above [`MAX_MERGE_QUEUE_ATTEMPT_DURATION`] update only the
    /// bounded `invalid_duration` transition-failure counter.
    pub fn record_merge_queue_completion(
        &self,
        completion: MergeQueueCompletion,
        duration: time::Duration,
    ) {
        if !(time::Duration::ZERO..=MAX_MERGE_QUEUE_ATTEMPT_DURATION).contains(&duration) {
            self.record_merge_queue_transition_failure(
                QueueTransitionFailureReason::InvalidDuration,
            );
            return;
        }

        let labels = completion.labels();
        self.inner
            .merge_queue_pr_outcomes
            .get_or_create(&labels)
            .inc();
        self.inner
            .merge_queue_attempt_duration
            .get_or_create(&MergeQueueDurationLabels {
                outcome: labels.outcome,
            })
            .observe(duration.as_seconds_f64());
    }

    /// Increments the merge-queue transition-failure total for a bounded reason.
    pub fn record_merge_queue_transition_failure(&self, reason: QueueTransitionFailureReason) {
        self.inner
            .merge_queue_transition_failures
            .get_or_create(&QueueTransitionFailureLabels { reason })
            .inc();
    }

    /// Observes the step count reported by one structurally valid completed workflow job.
    pub(crate) fn observe_workflow_job_steps(&self, step_count: usize) {
        self.inner.workflow_job_steps.observe(step_count as f64);
    }

    /// Increments the bounded counter for one rejected workflow-job trace.
    pub(crate) fn record_workflow_trace_rejection(&self, reason: WorkflowTraceRejectionReason) {
        self.inner
            .workflow_trace_rejections
            .get_or_create(&WorkflowTraceRejectionLabels { reason })
            .inc();
    }

    /// Replaces the configured-repository gauge with the supplied durable record count.
    pub fn set_repository_configurations(&self, count: u64) {
        self.inner.repository_configurations.set(count);
    }

    /// Increments the configured-repository gauge after a committed create.
    pub fn increment_repository_configurations(&self) {
        self.inner.repository_configurations.inc();
    }

    /// Decrements the configured-repository gauge after a committed delete.
    pub fn decrement_repository_configurations(&self) {
        self.inner.repository_configurations.dec();
    }

    pub(crate) fn encode(&self) -> Result<String, fmt::Error> {
        let mut exposition = String::new();
        encode(&mut exposition, &self.inner.registry)?;
        Ok(exposition)
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Builds the unauthenticated Prometheus exposition router.
pub fn router() -> Router<AppState> {
    Router::new().route("/metrics", get(metrics_handler))
}

async fn metrics_handler(State(state): State<AppState>) -> Result<Response, AppError> {
    let exposition = state.metrics().encode().map_err(AppError::internal)?;
    Response::builder()
        .status(StatusCode::OK)
        .header(CONTENT_TYPE, OPEN_METRICS_CONTENT_TYPE)
        .body(Body::from(exposition))
        .map_err(AppError::internal)
}

fn request_duration_histogram() -> Histogram {
    Histogram::new(REQUEST_DURATION_BUCKETS)
}

fn request_body_size_histogram() -> Histogram {
    Histogram::new(REQUEST_BODY_SIZE_BUCKETS)
}

fn merge_queue_attempt_duration_histogram() -> Histogram {
    Histogram::new(MERGE_QUEUE_ATTEMPT_DURATION_BUCKETS)
}

/// Normalizes an untrusted GitHub event header using the fixed v1 allowlist.
///
/// Values are matched exactly and case-sensitively. Any value outside the allowlist maps to
/// [`EventType::Other`], so untrusted input can never create a new label value.
pub fn normalize_event_type(raw_event_type: &str) -> EventType {
    match raw_event_type {
        "branch_protection_rule" => EventType::BranchProtectionRule,
        "check_run" => EventType::CheckRun,
        "check_suite" => EventType::CheckSuite,
        "create" => EventType::Create,
        "delete" => EventType::Delete,
        "deployment" => EventType::Deployment,
        "deployment_status" => EventType::DeploymentStatus,
        "discussion" => EventType::Discussion,
        "discussion_comment" => EventType::DiscussionComment,
        "issues" => EventType::Issues,
        "issue_comment" => EventType::IssueComment,
        "merge_group" => EventType::MergeGroup,
        "pull_request" => EventType::PullRequest,
        "pull_request_review" => EventType::PullRequestReview,
        "pull_request_review_comment" => EventType::PullRequestReviewComment,
        "push" => EventType::Push,
        "release" => EventType::Release,
        "repository" => EventType::Repository,
        "status" => EventType::Status,
        "workflow_job" => EventType::WorkflowJob,
        "workflow_run" => EventType::WorkflowRun,
        _ => EventType::Other,
    }
}

/// Normalizes an untrusted merge-group destroyed reason using an exact fixed mapping.
///
/// Values are matched exactly and case-sensitively. Unsupported raw values are discarded and map
/// to [`MergeGroupReason::Other`].
pub fn normalize_merge_group_destroyed_reason(raw_reason: &str) -> MergeGroupReason {
    match raw_reason {
        "merged" => MergeGroupReason::Merged,
        "dequeued" => MergeGroupReason::Dequeued,
        "invalidated" => MergeGroupReason::Invalidated,
        _ => MergeGroupReason::Other,
    }
}

/// Normalizes an optional untrusted GitHub action using the fixed global v1 allowlist.
///
/// An absent action maps to [`Action::None`]. A present value is matched exactly and
/// case-sensitively; any value outside the allowlist maps to [`Action::Other`].
pub fn normalize_action(raw_action: Option<&str>) -> Action {
    match raw_action {
        None => Action::None,
        Some("assigned") => Action::Assigned,
        Some("checks_requested") => Action::ChecksRequested,
        Some("closed") => Action::Closed,
        Some("completed") => Action::Completed,
        Some("created") => Action::Created,
        Some("deleted") => Action::Deleted,
        Some("dequeued") => Action::Dequeued,
        Some("destroyed") => Action::Destroyed,
        Some("edited") => Action::Edited,
        Some("enqueued") => Action::Enqueued,
        Some("in_progress") => Action::InProgress,
        Some("labeled") => Action::Labeled,
        Some("opened") => Action::Opened,
        Some("published") => Action::Published,
        Some("queued") => Action::Queued,
        Some("reopened") => Action::Reopened,
        Some("requested") => Action::Requested,
        Some("requested_action") => Action::RequestedAction,
        Some("rerequested") => Action::Rerequested,
        Some("submitted") => Action::Submitted,
        Some("synchronize") => Action::Synchronize,
        Some("unassigned") => Action::Unassigned,
        Some("unlabeled") => Action::Unlabeled,
        Some("unpublished") => Action::Unpublished,
        Some("updated") => Action::Updated,
        Some("waiting") => Action::Waiting,
        Some(_) => Action::Other,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{
        normalize_action, normalize_event_type, normalize_merge_group_destroyed_reason, Action,
        EventType, FailureStage, MergeGroupAction, MergeGroupReason, MergeQueueCompletion,
        MergeQueueOutcome, MergeQueueReason, Metrics, QueueTransitionFailureReason, WebhookResult,
        WorkflowTraceRejectionReason,
    };

    #[test]
    fn normalization_preserves_every_allowed_event_type() {
        let cases = [
            ("branch_protection_rule", EventType::BranchProtectionRule),
            ("check_run", EventType::CheckRun),
            ("check_suite", EventType::CheckSuite),
            ("create", EventType::Create),
            ("delete", EventType::Delete),
            ("deployment", EventType::Deployment),
            ("deployment_status", EventType::DeploymentStatus),
            ("discussion", EventType::Discussion),
            ("discussion_comment", EventType::DiscussionComment),
            ("issues", EventType::Issues),
            ("issue_comment", EventType::IssueComment),
            ("merge_group", EventType::MergeGroup),
            ("pull_request", EventType::PullRequest),
            ("pull_request_review", EventType::PullRequestReview),
            (
                "pull_request_review_comment",
                EventType::PullRequestReviewComment,
            ),
            ("push", EventType::Push),
            ("release", EventType::Release),
            ("repository", EventType::Repository),
            ("status", EventType::Status),
            ("workflow_job", EventType::WorkflowJob),
            ("workflow_run", EventType::WorkflowRun),
        ];

        for (raw_event_type, expected) in cases {
            assert_eq!(normalize_event_type(raw_event_type), expected);
        }
    }

    #[test]
    fn normalization_maps_unrecognized_event_types_to_other() {
        for raw_event_type in ["", "Push", "unknown", "push\nrepository=private/repository"] {
            assert_eq!(normalize_event_type(raw_event_type), EventType::Other);
        }
    }

    #[test]
    fn normalization_preserves_every_allowed_action() {
        let cases = [
            ("assigned", Action::Assigned),
            ("checks_requested", Action::ChecksRequested),
            ("closed", Action::Closed),
            ("completed", Action::Completed),
            ("created", Action::Created),
            ("deleted", Action::Deleted),
            ("dequeued", Action::Dequeued),
            ("destroyed", Action::Destroyed),
            ("edited", Action::Edited),
            ("enqueued", Action::Enqueued),
            ("in_progress", Action::InProgress),
            ("labeled", Action::Labeled),
            ("opened", Action::Opened),
            ("published", Action::Published),
            ("queued", Action::Queued),
            ("reopened", Action::Reopened),
            ("requested", Action::Requested),
            ("requested_action", Action::RequestedAction),
            ("rerequested", Action::Rerequested),
            ("submitted", Action::Submitted),
            ("synchronize", Action::Synchronize),
            ("unassigned", Action::Unassigned),
            ("unlabeled", Action::Unlabeled),
            ("unpublished", Action::Unpublished),
            ("updated", Action::Updated),
            ("waiting", Action::Waiting),
        ];

        for (raw_action, expected) in cases {
            assert_eq!(normalize_action(Some(raw_action)), expected);
        }
    }

    #[test]
    fn normalization_distinguishes_missing_and_unrecognized_actions() {
        assert_eq!(normalize_action(None), Action::None);

        for raw_action in [
            "",
            "Opened",
            "unknown",
            "opened\nsignature=attacker-controlled",
        ] {
            assert_eq!(normalize_action(Some(raw_action)), Action::Other);
        }
    }

    #[test]
    fn metric_updates_record_every_required_instrument() {
        let metrics = Metrics::new();

        metrics.observe_request(WebhookResult::Accepted, Duration::from_millis(25));
        metrics.observe_event(EventType::Push, Action::None, 1_024);
        metrics.record_duplicate();
        metrics.record_failure(FailureStage::Metrics);
        metrics.set_repository_configurations(7);

        let exposition = metrics.encode().expect("metrics encode into a String");
        for expected_sample in [
            "github_webhook_requests_total{result=\"accepted\"} 1",
            "github_webhook_events_total{event_type=\"push\",action=\"none\"} 1",
            "github_webhook_processing_duration_seconds_count{result=\"accepted\"} 1",
            "github_webhook_processing_duration_seconds_sum{result=\"accepted\"} 0.025",
            "github_webhook_request_body_bytes_count 1",
            "github_webhook_request_body_bytes_sum 1024.0",
            "github_webhook_duplicates_total 1",
            "github_webhook_processing_failures_total{stage=\"metrics\"} 1",
            "github_repository_configurations 7",
        ] {
            assert!(
                exposition.contains(expected_sample),
                "missing sample {expected_sample:?} in:\n{exposition}"
            );
        }
    }

    #[test]
    fn workflow_job_metrics_observe_sizes_and_bounded_rejections() {
        let metrics = Metrics::new();

        metrics.observe_workflow_job_steps(0);
        metrics.observe_workflow_job_steps(36);
        metrics.observe_workflow_job_steps(1_500);
        metrics.record_workflow_trace_rejection(WorkflowTraceRejectionReason::TooManySteps);

        let exposition = metrics.encode().expect("metrics encode");
        for sample in [
            "github_workflow_job_steps_bucket{le=\"0.0\"} 1",
            "github_workflow_job_steps_bucket{le=\"40.0\"} 2",
            "github_workflow_job_steps_bucket{le=\"1024.0\"} 2",
            "github_workflow_job_steps_bucket{le=\"+Inf\"} 3",
            "github_workflow_job_steps_count 3",
            "github_workflow_job_steps_sum 1536.0",
            "github_workflow_job_trace_rejections_total{reason=\"too_many_steps\"} 1",
        ] {
            assert!(
                exposition.contains(sample),
                "missing {sample:?} in:\n{exposition}"
            );
        }
    }

    #[test]
    fn every_result_and_failure_stage_encodes_to_the_fixed_vocabulary() {
        let metrics = Metrics::new();
        let results = [
            (WebhookResult::Accepted, "accepted"),
            (WebhookResult::Malformed, "malformed"),
            (WebhookResult::Unauthorized, "unauthorized"),
            (WebhookResult::TooLarge, "too_large"),
            (WebhookResult::Unsupported, "unsupported"),
            (WebhookResult::Unavailable, "unavailable"),
        ];
        let stages = [
            (FailureStage::Authentication, "authentication"),
            (FailureStage::DeliveryClaim, "delivery_claim"),
            (FailureStage::Metrics, "metrics"),
            (FailureStage::Database, "database"),
        ];

        for (result, _) in results {
            metrics.observe_request(result, Duration::ZERO);
        }
        for (stage, _) in stages {
            metrics.record_failure(stage);
        }

        let exposition = metrics.encode().expect("metrics encode into a String");
        for (_, result) in results {
            assert!(exposition.contains(&format!("result=\"{result}\"")));
        }
        for (_, stage) in stages {
            assert!(exposition.contains(&format!("stage=\"{stage}\"")));
        }
    }

    #[test]
    fn clones_share_one_registry_during_concurrent_updates() {
        const THREADS: usize = 8;
        const UPDATES_PER_THREAD: usize = 100;
        let metrics = Metrics::new();
        let mut workers = Vec::with_capacity(THREADS);

        for _ in 0..THREADS {
            let worker_metrics = metrics.clone();
            workers.push(std::thread::spawn(move || {
                for _ in 0..UPDATES_PER_THREAD {
                    worker_metrics.record_duplicate();
                    worker_metrics.observe_event(EventType::Push, Action::None, 128);
                    worker_metrics.observe_workflow_job_steps(1);
                    worker_metrics.record_workflow_trace_rejection(
                        WorkflowTraceRejectionReason::TooManySteps,
                    );
                }
            }));
        }
        for worker in workers {
            worker.join().expect("metrics worker completes");
        }

        let exposition = metrics.encode().expect("metrics encode into a String");
        let expected_updates = THREADS * UPDATES_PER_THREAD;
        assert!(exposition.contains(&format!(
            "github_webhook_duplicates_total {expected_updates}"
        )));
        assert!(exposition.contains(&format!(
            "github_webhook_events_total{{event_type=\"push\",action=\"none\"}} \
             {expected_updates}"
        )));
        assert!(exposition.contains(&format!(
            "github_webhook_request_body_bytes_count {expected_updates}"
        )));
        assert!(exposition.contains(&format!(
            "github_workflow_job_steps_count {expected_updates}"
        )));
        assert!(exposition.contains(&format!(
            "github_workflow_job_trace_rejections_total{{reason=\"too_many_steps\"}} \
             {expected_updates}"
        )));
    }

    #[test]
    fn normalize_merge_group_destroyed_reason_uses_exact_bounded_mapping() {
        let recognized = [
            ("merged", MergeGroupReason::Merged),
            ("dequeued", MergeGroupReason::Dequeued),
            ("invalidated", MergeGroupReason::Invalidated),
        ];

        for (raw_reason, expected) in recognized {
            assert_eq!(normalize_merge_group_destroyed_reason(raw_reason), expected);
        }
        for raw_reason in [
            "",
            "Merged",
            "unknown",
            "merged\nrepository=private/repository",
        ] {
            assert_eq!(
                normalize_merge_group_destroyed_reason(raw_reason),
                MergeGroupReason::Other
            );
        }
    }

    #[test]
    fn phase_three_vocabularies_encode_only_fixed_label_values() {
        let metrics = Metrics::new();

        for (action, reason, encoded_action, encoded_reason) in [
            (
                MergeGroupAction::ChecksRequested,
                MergeGroupReason::Other,
                "checks_requested",
                "none",
            ),
            (
                MergeGroupAction::Destroyed,
                MergeGroupReason::Merged,
                "destroyed",
                "merged",
            ),
            (
                MergeGroupAction::Destroyed,
                MergeGroupReason::Dequeued,
                "destroyed",
                "dequeued",
            ),
            (
                MergeGroupAction::Destroyed,
                MergeGroupReason::Invalidated,
                "destroyed",
                "invalidated",
            ),
            (
                MergeGroupAction::Destroyed,
                MergeGroupReason::Other,
                "destroyed",
                "other",
            ),
        ] {
            metrics.record_merge_group_event(action, reason);
            let exposition = metrics.encode().expect("metrics encode into a String");
            assert!(exposition.contains(&format!(
                "github_merge_group_events_total{{action=\"{encoded_action}\",reason=\"{encoded_reason}\"}} 1"
            )));
        }

        for (outcome, encoded_outcome) in [
            (MergeQueueOutcome::Succeeded, "succeeded"),
            (MergeQueueOutcome::Failed, "failed"),
            (MergeQueueOutcome::Cancelled, "cancelled"),
            (MergeQueueOutcome::Unknown, "unknown"),
        ] {
            assert_eq!(outcome.as_str(), encoded_outcome);
        }
        for (reason, encoded_reason) in [
            (MergeQueueReason::PullRequestMerged, "pull_request_merged"),
            (
                MergeQueueReason::UnclassifiedDequeue,
                "unclassified_dequeue",
            ),
        ] {
            assert_eq!(reason.as_str(), encoded_reason);
        }
        for (completion, encoded_outcome, encoded_reason) in [
            (
                MergeQueueCompletion::PullRequestMerged,
                "succeeded",
                "pull_request_merged",
            ),
            (
                MergeQueueCompletion::UnclassifiedDequeue,
                "unknown",
                "unclassified_dequeue",
            ),
        ] {
            metrics.record_merge_queue_completion(completion, time::Duration::seconds(1));
            let exposition = metrics.encode().expect("metrics encode into a String");
            assert!(exposition.contains(&format!(
                "github_merge_queue_pr_outcomes_total{{outcome=\"{encoded_outcome}\",reason=\"{encoded_reason}\"}} 1"
            )));
        }

        for (reason, encoded_reason) in [
            (
                QueueTransitionFailureReason::MissingActiveAttempt,
                "missing_active_attempt",
            ),
            (
                QueueTransitionFailureReason::InvalidDuration,
                "invalid_duration",
            ),
        ] {
            metrics.record_merge_queue_transition_failure(reason);
            let exposition = metrics.encode().expect("metrics encode into a String");
            assert!(exposition.contains(&format!(
                "github_merge_queue_transition_failures_total{{reason=\"{encoded_reason}\"}} 1"
            )));
        }

        metrics.record_failure(FailureStage::QueueState);
        assert!(metrics
            .encode()
            .expect("metrics encode into a String")
            .contains("github_webhook_processing_failures_total{stage=\"queue_state\"} 1"));
    }

    #[test]
    fn valid_queue_completions_update_outcome_and_duration_once() {
        let metrics = Metrics::new();

        metrics.record_merge_queue_completion(
            MergeQueueCompletion::PullRequestMerged,
            time::Duration::seconds(90),
        );

        let exposition = metrics.encode().expect("metrics encode into a String");
        assert!(exposition.contains(
            "github_merge_queue_pr_outcomes_total{outcome=\"succeeded\",reason=\"pull_request_merged\"} 1"
        ));
        assert!(exposition.contains(
            "github_merge_queue_attempt_duration_seconds_count{outcome=\"succeeded\"} 1"
        ));
        assert!(exposition.contains(
            "github_merge_queue_attempt_duration_seconds_sum{outcome=\"succeeded\"} 90.0"
        ));
        assert!(exposition.contains(
            "github_merge_queue_transition_failures_total{reason=\"invalid_duration\"} 0"
        ));
    }

    #[test]
    fn queue_duration_sanity_ceiling_is_inclusive() {
        let metrics = Metrics::new();

        metrics.record_merge_queue_completion(
            MergeQueueCompletion::PullRequestMerged,
            time::Duration::days(365),
        );

        let exposition = metrics.encode().expect("metrics encode into a String");
        assert!(exposition.contains(
            "github_merge_queue_attempt_duration_seconds_count{outcome=\"succeeded\"} 1"
        ));
        assert!(exposition.contains(
            "github_merge_queue_transition_failures_total{reason=\"invalid_duration\"} 0"
        ));
    }

    #[test]
    fn long_queue_durations_retain_finite_histogram_resolution() {
        let metrics = Metrics::new();

        metrics.record_merge_queue_completion(
            MergeQueueCompletion::UnclassifiedDequeue,
            time::Duration::days(30),
        );

        let exposition = metrics.encode().expect("metrics encode into a String");
        for expected_sample in [
            "github_merge_queue_attempt_duration_seconds_bucket{le=\"2592000.0\",outcome=\"unknown\"} 1",
            "github_merge_queue_attempt_duration_seconds_bucket{le=\"31536000.0\",outcome=\"unknown\"} 1",
        ] {
            assert!(
                exposition.contains(expected_sample),
                "missing sample {expected_sample:?} in:\n{exposition}"
            );
        }
    }

    #[test]
    fn invalid_queue_durations_update_only_the_failure_metric() {
        let metrics = Metrics::new();

        for invalid_duration in [
            time::Duration::nanoseconds(-1),
            time::Duration::days(365) + time::Duration::nanoseconds(1),
        ] {
            metrics.record_merge_queue_completion(
                MergeQueueCompletion::UnclassifiedDequeue,
                invalid_duration,
            );
        }

        let exposition = metrics.encode().expect("metrics encode into a String");
        assert!(exposition.contains(
            "github_merge_queue_pr_outcomes_total{outcome=\"unknown\",reason=\"unclassified_dequeue\"} 0"
        ));
        assert!(exposition
            .contains("github_merge_queue_attempt_duration_seconds_count{outcome=\"unknown\"} 0"));
        assert!(exposition.contains(
            "github_merge_queue_transition_failures_total{reason=\"invalid_duration\"} 2"
        ));
    }

    #[test]
    fn phase_three_updates_are_shared_across_concurrent_clones() {
        const THREADS: usize = 8;
        let metrics = Metrics::new();
        let workers = (0..THREADS)
            .map(|_| {
                let worker_metrics = metrics.clone();
                std::thread::spawn(move || {
                    worker_metrics.record_merge_group_event(
                        MergeGroupAction::Destroyed,
                        MergeGroupReason::Merged,
                    );
                    worker_metrics.record_merge_queue_completion(
                        MergeQueueCompletion::PullRequestMerged,
                        time::Duration::seconds(1),
                    );
                })
            })
            .collect::<Vec<_>>();

        for worker in workers {
            worker.join().expect("metrics worker completes");
        }

        let exposition = metrics.encode().expect("metrics encode into a String");
        assert!(exposition.contains(&format!(
            "github_merge_group_events_total{{action=\"destroyed\",reason=\"merged\"}} {THREADS}"
        )));
        assert!(exposition.contains(&format!(
            "github_merge_queue_pr_outcomes_total{{outcome=\"succeeded\",reason=\"pull_request_merged\"}} {THREADS}"
        )));
        assert!(exposition.contains(&format!(
            "github_merge_queue_attempt_duration_seconds_count{{outcome=\"succeeded\"}} {THREADS}"
        )));
    }

    #[test]
    fn phase_three_metrics_exist_at_startup() {
        let exposition = Metrics::new()
            .encode()
            .expect("metrics encode into a String");

        for metric_name in [
            "github_merge_group_events_total",
            "github_merge_queue_pr_outcomes_total",
            "github_merge_queue_attempt_duration_seconds",
            "github_merge_queue_transition_failures_total",
            "github_workflow_job_steps",
            "github_workflow_job_trace_rejections_total",
        ] {
            assert!(
                exposition.contains(metric_name),
                "missing {metric_name:?} in:\n{exposition}"
            );
        }
    }

    #[test]
    fn metric_updates_never_expose_untrusted_values() {
        let forbidden_values = [
            "private/repository",
            "8f14e45f-ea58-4c6d-b70f-4c32af72b405",
            "payload-secret-fragment",
            "sha256=attacker-signature",
            "https://example.invalid/private",
            "0123456789abcdef0123456789abcdef01234567",
            "pull-request-4815162342",
        ];
        let raw_event = format!("unknown-{}", forbidden_values.join("-"));
        let raw_action = format!("unknown-{}", forbidden_values.join("-"));
        let raw_group_reason = format!("unknown-{}", forbidden_values.join("-"));
        let metrics = Metrics::new();

        metrics.observe_event(
            normalize_event_type(&raw_event),
            normalize_action(Some(&raw_action)),
            64,
        );
        metrics.record_merge_group_event(
            MergeGroupAction::Destroyed,
            normalize_merge_group_destroyed_reason(&raw_group_reason),
        );
        metrics.observe_workflow_job_steps(36);
        metrics.record_workflow_trace_rejection(WorkflowTraceRejectionReason::TooManySteps);

        let exposition = metrics.encode().expect("metrics encode into a String");
        assert!(exposition
            .contains("github_webhook_events_total{event_type=\"other\",action=\"other\"} 1"));
        assert!(exposition.contains("github_workflow_job_steps_count 1"));
        assert!(exposition
            .contains("github_workflow_job_trace_rejections_total{reason=\"too_many_steps\"} 1"));
        for forbidden_value in forbidden_values {
            assert!(!exposition.contains(forbidden_value));
        }
        assert!(!exposition.contains(&raw_event));
        assert!(!exposition.contains(&raw_action));
        assert!(!exposition.contains(&raw_group_reason));
    }
}
