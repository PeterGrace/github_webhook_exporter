//! Centralized trace policy for bounded span names, statuses, and identifiers.

use std::fmt;

use axum::extract::MatchedPath;
use http::{Method, StatusCode};
use opentelemetry::{trace::Status, Array, KeyValue, Value};
use thiserror::Error;
use tracing::{info_span, Instrument, Span};
use tracing_opentelemetry::OpenTelemetrySpanExt;

use crate::domain::{
    delivery::DeliveryId, merge_queue::PullRequestNumber, repository::RepositoryId,
};
use crate::metrics::{
    Action, EventType, MergeGroupAction, MergeGroupReason, MergeQueueCompletion, MergeQueueOutcome,
    MergeQueueReason,
};
use crate::security::CanonicalRepositoryName;
use crate::telemetry::workflow::{
    TimingSource, WorkflowBranch, WorkflowConclusion, WorkflowEvent, WorkflowJobId,
    WorkflowRunAttempt, WorkflowRunId, WorkflowStepTrace,
};

const TELEMETRY_TARGET: &str = "github_webhook_exporter";
const SQLITE_SYSTEM_NAME: &str = "sqlite";
const OPERATION_FAILURE_EVENT: &str = "operation.failure";
const OPERATION_OUTCOME_KEY: &str = "ghe.operation.outcome";
const FAILURE_REASON_KEY: &str = "ghe.failure.reason";
const HTTP_REQUEST_METHOD_KEY: &str = "http.request.method";
const HTTP_ROUTE_KEY: &str = "http.route";
const HTTP_RESPONSE_STATUS_CODE_KEY: &str = "http.response.status_code";
const HTTP_RESULT_KEY: &str = "ghe.http.result";
const CONFIG_OPERATION_KEY: &str = "ghe.config.operation";
const WEBHOOK_EVENT_TYPE_KEY: &str = "ghe.webhook.event_type";
const WEBHOOK_ACTION_KEY: &str = "ghe.webhook.action";
const QUEUE_ENTITY_KEY: &str = "ghe.queue.entity";
const MERGE_GROUP_ACTION_KEY: &str = "ghe.merge_group.action";
const MERGE_GROUP_REASON_KEY: &str = "ghe.merge_group.reason";
const QUEUE_OUTCOME_KEY: &str = "ghe.queue.outcome";
const QUEUE_REASON_KEY: &str = "ghe.queue.reason";
const REPOSITORY_NAME_KEY: &str = "github.repository.name";
const REPOSITORY_ID_KEY: &str = "github.repository.id";
const DELIVERY_ID_KEY: &str = "github.delivery.id";
const PULL_REQUEST_NUMBER_KEY: &str = "github.pull_request.number";
const COMMIT_SHA_KEY: &str = "github.commit.sha";
const DB_SYSTEM_NAME_KEY: &str = "db.system.name";
const DB_OPERATION_NAME_KEY: &str = "db.operation.name";
const SHA_1_HEX_LENGTH: usize = 40;
const SHA_256_HEX_LENGTH: usize = 64;
const CICD_PIPELINE_NAME_KEY: &str = "cicd.pipeline.name";
const CICD_PIPELINE_RUN_ID_KEY: &str = "cicd.pipeline.run.id";
const CICD_PIPELINE_RUN_URL_KEY: &str = "cicd.pipeline.run.url.full";
const CICD_PIPELINE_TASK_NAME_KEY: &str = "cicd.pipeline.task.name";
const CICD_PIPELINE_TASK_RUN_ID_KEY: &str = "cicd.pipeline.task.run.id";
const CICD_PIPELINE_TASK_RUN_RESULT_KEY: &str = "cicd.pipeline.task.run.result";
const CICD_PIPELINE_TASK_RUN_URL_KEY: &str = "cicd.pipeline.task.run.url.full";
const ERROR_TYPE_KEY: &str = "error.type";
const SENTRY_DESCRIPTION_KEY: &str = "sentry.description";
const SENTRY_OPERATION_KEY: &str = "sentry.op";
const VCS_HEAD_REVISION_KEY: &str = "vcs.ref.head.revision";
const VCS_REPOSITORY_NAME_KEY: &str = "vcs.repository.name";
const VCS_REPOSITORY_URL_KEY: &str = "vcs.repository.url.full";
const GITHUB_WORKFLOW_CONCLUSION_KEY: &str = "github.workflow.conclusion";
const GITHUB_WORKFLOW_RUN_ATTEMPT_KEY: &str = "github.workflow.run.attempt";
const GITHUB_WORKFLOW_EVENT_KEY: &str = "github.workflow.event";
const GITHUB_WORKFLOW_SOURCE_BRANCH_KEY: &str = "github.workflow.source_branch";
const GITHUB_WORKFLOW_TARGET_BRANCH_KEY: &str = "github.workflow.target_branch";
const TIMING_SOURCE_KEY: &str = "timing_source";

pub(super) const GITHUB_ACTIONS_PIPELINE_OPERATION: &str = "github.actions.pipeline";
pub(super) const GITHUB_ACTIONS_PIPELINE_TASK_OPERATION: &str = "github.actions.pipeline.task";
pub(super) const GITHUB_ACTIONS_JOB_OPERATION: &str = "github.actions.job";
pub(super) const GITHUB_ACTIONS_STEP_OPERATION: &str = "github.actions.step";

fn string_key_value(key: &'static str, value: impl Into<String>) -> KeyValue {
    KeyValue::new(key, value.into())
}

fn decimal_string_key_value(key: &'static str, value: i64) -> KeyValue {
    KeyValue::new(key, value.to_string())
}

fn set_key_value(span: &Span, attribute: KeyValue) {
    span.set_attribute(attribute.key, attribute.value);
}

/// Returns the canonical repository-name attribute.
pub(crate) fn repository_name_attribute(name: &CanonicalRepositoryName) -> KeyValue {
    string_key_value(REPOSITORY_NAME_KEY, name.as_str())
}

/// Returns the delivery identifier attribute.
pub(crate) fn delivery_id_attribute(id: &DeliveryId) -> KeyValue {
    let mut buffer = uuid::Uuid::encode_buffer();
    string_key_value(DELIVERY_ID_KEY, id.encode_lower(&mut buffer).to_owned())
}

/// Returns the pull-request number attribute.
pub(crate) fn pull_request_number_attribute(number: PullRequestNumber) -> KeyValue {
    KeyValue::new(PULL_REQUEST_NUMBER_KEY, number.get())
}

/// Returns the bounded pull-request number collection attribute for historical workflow roots.
pub(crate) fn pull_request_numbers_attribute(numbers: &[PullRequestNumber]) -> Option<KeyValue> {
    if numbers.is_empty() {
        None
    } else {
        Some(KeyValue::new(
            PULL_REQUEST_NUMBER_KEY,
            Value::Array(Array::I64(
                numbers.iter().map(|number| number.get()).collect(),
            )),
        ))
    }
}

/// Returns the commit-SHA attribute.
pub(crate) fn commit_sha_attribute(sha: &CommitSha) -> KeyValue {
    string_key_value(COMMIT_SHA_KEY, sha.as_str())
}

/// Returns the operation-outcome attribute.
pub(crate) fn operation_outcome_attribute(outcome: OperationOutcome) -> KeyValue {
    string_key_value(OPERATION_OUTCOME_KEY, outcome.as_str())
}

/// Returns the workflow display-name attribute.
pub(crate) fn workflow_name_attribute(name: &str) -> KeyValue {
    string_key_value(CICD_PIPELINE_NAME_KEY, name)
}

/// Returns the semantic-convention workflow run identifier attribute.
pub(crate) fn workflow_pipeline_run_id_attribute(run_id: WorkflowRunId) -> KeyValue {
    decimal_string_key_value(CICD_PIPELINE_RUN_ID_KEY, run_id.get())
}

/// Returns the workflow run-attempt attribute.
pub(crate) fn workflow_run_attempt_attribute(run_attempt: WorkflowRunAttempt) -> KeyValue {
    decimal_string_key_value(GITHUB_WORKFLOW_RUN_ATTEMPT_KEY, run_attempt.get())
}

/// Returns the normalized workflow trigger attribute.
pub(crate) fn workflow_event_attribute(event: WorkflowEvent) -> KeyValue {
    string_key_value(GITHUB_WORKFLOW_EVENT_KEY, event.as_str())
}

/// Returns a sanitized workflow source-branch attribute.
pub(crate) fn workflow_source_branch_attribute(branch: &WorkflowBranch) -> KeyValue {
    string_key_value(GITHUB_WORKFLOW_SOURCE_BRANCH_KEY, branch.as_str())
}

/// Returns a sanitized workflow target-branch attribute.
pub(crate) fn workflow_target_branch_attribute(branch: &WorkflowBranch) -> KeyValue {
    string_key_value(GITHUB_WORKFLOW_TARGET_BRANCH_KEY, branch.as_str())
}

/// Returns the semantic-convention URL for a GitHub Actions workflow run.
pub(crate) fn workflow_pipeline_run_url_attribute(
    repository: &CanonicalRepositoryName,
    run_id: WorkflowRunId,
) -> KeyValue {
    string_key_value(
        CICD_PIPELINE_RUN_URL_KEY,
        format!(
            "https://github.com/{}/actions/runs/{}",
            repository.as_str(),
            run_id.get()
        ),
    )
}

/// Returns the semantic-convention URL for a GitHub Actions job task run.
pub(crate) fn workflow_task_run_url_attribute(
    repository: &CanonicalRepositoryName,
    run_id: WorkflowRunId,
    job_id: WorkflowJobId,
) -> KeyValue {
    string_key_value(
        CICD_PIPELINE_TASK_RUN_URL_KEY,
        format!(
            "https://github.com/{}/actions/runs/{}/job/{}",
            repository.as_str(),
            run_id.get(),
            job_id.get()
        ),
    )
}

/// Returns the semantic-convention URL for a GitHub Actions step task run.
pub(crate) fn workflow_step_task_run_url_attribute(
    repository: &CanonicalRepositoryName,
    run_id: WorkflowRunId,
    job_id: WorkflowJobId,
    step: &WorkflowStepTrace,
) -> KeyValue {
    string_key_value(
        CICD_PIPELINE_TASK_RUN_URL_KEY,
        format!(
            "https://github.com/{}/actions/runs/{}/job/{}#step:{}:1",
            repository.as_str(),
            run_id.get(),
            job_id.get(),
            step.number()
        ),
    )
}

/// Returns the semantic-convention repository URL for a workflow task.
pub(crate) fn workflow_repository_url_attribute(repository: &CanonicalRepositoryName) -> KeyValue {
    string_key_value(
        VCS_REPOSITORY_URL_KEY,
        format!("https://github.com/{}", repository.as_str()),
    )
}

/// Returns the semantic-convention repository name for a workflow task.
pub(crate) fn workflow_repository_name_attribute(repository: &CanonicalRepositoryName) -> KeyValue {
    // The fallback is defensive because CanonicalRepositoryName currently guarantees owner/name.
    let repository_name = repository
        .as_str()
        .rsplit_once('/')
        .map_or(repository.as_str(), |(_, name)| name);
    string_key_value(VCS_REPOSITORY_NAME_KEY, repository_name)
}

/// Returns the semantic-convention head revision for a workflow task.
pub(crate) fn workflow_head_revision_attribute(sha: &CommitSha) -> KeyValue {
    string_key_value(VCS_HEAD_REVISION_KEY, sha.as_str())
}

/// Returns the semantic-convention task-run identifier for a workflow job root.
pub(crate) fn workflow_pipeline_task_run_id_attribute(job_id: WorkflowJobId) -> KeyValue {
    decimal_string_key_value(CICD_PIPELINE_TASK_RUN_ID_KEY, job_id.get())
}

/// Returns the semantic-convention task-run identifier for a workflow step.
pub(crate) fn workflow_pipeline_step_task_run_id_attribute(
    job_id: WorkflowJobId,
    step: &WorkflowStepTrace,
) -> KeyValue {
    string_key_value(
        CICD_PIPELINE_TASK_RUN_ID_KEY,
        format!("{}:{}", job_id.get(), step.number()),
    )
}

/// Returns the workflow job or step display-name attribute.
pub(crate) fn workflow_task_name_attribute(name: &str) -> KeyValue {
    string_key_value(CICD_PIPELINE_TASK_NAME_KEY, name)
}

/// Returns the workflow conclusion attribute.
pub(crate) fn workflow_conclusion_attribute(conclusion: WorkflowConclusion) -> KeyValue {
    string_key_value(GITHUB_WORKFLOW_CONCLUSION_KEY, conclusion.as_str())
}

/// Returns the bounded semantic-convention task-run result attribute.
pub(crate) fn workflow_pipeline_task_run_result_attribute(
    conclusion: WorkflowConclusion,
) -> KeyValue {
    string_key_value(
        CICD_PIPELINE_TASK_RUN_RESULT_KEY,
        conclusion.semantic_result(),
    )
}

/// Returns the bounded error type for a failed or timed-out task.
pub(crate) fn workflow_error_type_attribute(conclusion: WorkflowConclusion) -> Option<KeyValue> {
    conclusion
        .error_type()
        .map(|error_type| string_key_value(ERROR_TYPE_KEY, error_type))
}

/// Returns an explicit Sentry operation for a workflow task span.
pub(crate) fn sentry_operation_attribute(operation: &'static str) -> KeyValue {
    string_key_value(SENTRY_OPERATION_KEY, operation)
}

/// Returns a bounded Sentry description for a workflow task span.
pub(crate) fn sentry_description_attribute(description: String) -> KeyValue {
    string_key_value(SENTRY_DESCRIPTION_KEY, description)
}

/// Returns the timing-source attribute.
pub(crate) fn timing_source_attribute(source: TimingSource) -> KeyValue {
    string_key_value(TIMING_SOURCE_KEY, source.as_str())
}

/// A bounded high-level operation recorded in tracing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    /// An HTTP request boundary.
    HttpRequest,
    /// Webhook authentication and repository authorization.
    WebhookAuthenticate,
    /// Webhook body decoding and processing.
    WebhookProcess,
    /// Repository configuration persistence.
    RepositoryWrite,
    /// A SQLite query boundary.
    SqliteQuery,
    /// A merge-queue state update.
    MergeQueueUpdate,
    /// A retention job run.
    RetentionRun,
}

/// A bounded terminal outcome recorded in tracing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationOutcome {
    /// The operation succeeded.
    Success,
    /// The operation was a duplicate.
    Duplicate,
    /// The operation made no durable change.
    NoOp,
    /// The operation was cancelled.
    Cancelled,
    /// The operation failed.
    Failure,
}

/// A bounded HTTP method recorded in tracing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HttpMethod {
    /// HTTP GET.
    Get,
    /// HTTP POST.
    Post,
    /// HTTP PATCH.
    Patch,
    /// HTTP DELETE.
    Delete,
    /// HTTP HEAD.
    Head,
    /// HTTP OPTIONS.
    Options,
    /// HTTP PUT.
    Put,
    /// Any method outside the approved telemetry vocabulary.
    Other,
}

/// A bounded HTTP response class recorded in tracing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HttpResult {
    /// HTTP informational, success, or redirect response.
    Success,
    /// HTTP client error response.
    ClientError,
    /// HTTP server error response.
    ServerError,
    /// Any extension status code outside the approved response classes.
    Other,
}

/// A bounded repository-configuration operation recorded in tracing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConfigOperation {
    /// Create a repository configuration.
    Create,
    /// Update a repository configuration.
    Update,
    /// Delete a repository configuration.
    Delete,
}

/// A bounded merge-queue entity recorded in tracing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum QueueEntity {
    /// A GitHub merge group.
    MergeGroup,
    /// A GitHub pull request.
    PullRequest,
}

/// A bounded operation failure reason recorded in tracing events.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationFailureReason {
    /// Durable merge-queue state could not be updated.
    QueueState,
}

/// A bounded commit SHA recorded in tracing.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct CommitSha(String);

impl CommitSha {
    /// Parses a full SHA-1 or SHA-256 ASCII hexadecimal commit identifier.
    ///
    /// # Errors
    ///
    /// Returns [`CommitShaError`] unless `value` contains exactly 40 or 64 ASCII hexadecimal
    /// characters.
    pub(crate) fn parse(value: &str) -> Result<Self, CommitShaError> {
        if !matches!(value.len(), SHA_1_HEX_LENGTH | SHA_256_HEX_LENGTH) {
            return Err(CommitShaError);
        }

        let mut normalized = String::with_capacity(value.len());
        for byte in value.bytes() {
            if !byte.is_ascii_hexdigit() {
                return Err(CommitShaError);
            }
            normalized.push(byte.to_ascii_lowercase() as char);
        }

        Ok(Self(normalized))
    }

    /// Returns the canonical lowercase hexadecimal commit SHA.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CommitSha {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CommitSha([REDACTED])")
    }
}

/// A malformed commit SHA.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("commit SHA is not a valid 40- or 64-character hexadecimal string")]
pub(crate) struct CommitShaError;

/// A bounded SQLite operation recorded in tracing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DatabaseOperation {
    /// Counting repository rows.
    RepositoryCount,
    /// Creating a repository row.
    RepositoryCreate,
    /// Listing repository rows.
    RepositoryList,
    /// Authenticating a repository row.
    RepositoryAuthenticate,
    /// Loading one repository row.
    RepositoryGet,
    /// Updating one repository row.
    RepositoryUpdate,
    /// Deleting one repository row.
    RepositoryDelete,
    /// Claiming one delivery row.
    DeliveryClaim,
    /// Pruning processed delivery rows.
    DeliveryPrune,
    /// Enqueuing one merge-queue row.
    MergeQueueEnqueue,
    /// Completing one merge-queue row.
    MergeQueueComplete,
    /// Pruning merge-queue rows.
    MergeQueuePrune,
    /// Upserting workflow-run context.
    WorkflowRunUpsert,
    /// Loading workflow-run context.
    WorkflowRunGet,
    /// Pruning workflow-run context.
    WorkflowRunPrune,
    /// Recording one emitted workflow-job trace identity.
    WorkflowJobLinkRecord,
    /// Listing the emitted workflow-job trace identities of one run attempt.
    WorkflowJobLinkList,
    /// Pruning emitted workflow-job trace identities.
    WorkflowJobLinkPrune,
}

/// Creates a bounded tracing span for a high-level operation.
///
/// The span name is fixed by the operation vocabulary and never accepts caller-supplied text.
pub(crate) fn operation_span(operation: Operation) -> Span {
    match operation {
        Operation::HttpRequest => info_span!(target: TELEMETRY_TARGET, "http.request"),
        Operation::WebhookAuthenticate => {
            info_span!(target: TELEMETRY_TARGET, "github.webhook.authenticate")
        }
        Operation::WebhookProcess => {
            info_span!(target: TELEMETRY_TARGET, "github.webhook.process")
        }
        Operation::RepositoryWrite => {
            info_span!(target: TELEMETRY_TARGET, "config.repository.write")
        }
        Operation::SqliteQuery => info_span!(target: TELEMETRY_TARGET, "sqlite.query"),
        Operation::MergeQueueUpdate => {
            info_span!(target: TELEMETRY_TARGET, "merge_queue.update")
        }
        Operation::RetentionRun => {
            info_span!(target: TELEMETRY_TARGET, parent: None, "retention.run")
        }
    }
}

/// Creates a bounded tracing span for a SQLite query.
///
/// The returned span is named `sqlite.query` and is annotated only with fixed database metadata.
pub(crate) fn database_span(operation: DatabaseOperation) -> Span {
    let span = operation_span(Operation::SqliteQuery);
    span.set_attribute(DB_SYSTEM_NAME_KEY, SQLITE_SYSTEM_NAME);
    span.set_attribute(DB_OPERATION_NAME_KEY, operation.as_str());
    span
}

/// Instruments one logical SQLite store operation with a bounded database span.
///
/// # Parameters
///
/// * `operation` - The fixed database operation vocabulary value.
/// * `future` - The store operation future to execute inside the database span.
///
/// # Errors
///
/// Returns the exact error produced by `future` after recording only a bounded failure status.
pub(crate) async fn instrument_database_operation<T, E>(
    operation: DatabaseOperation,
    future: impl std::future::Future<Output = Result<T, E>>,
) -> Result<T, E> {
    let span = database_span(operation);
    let result = future.instrument(span.clone()).await;
    set_result_status(&span, &result);
    result
}

/// Records the bounded HTTP method as an OpenTelemetry span attribute.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `method` - The HTTP method to map into the fixed telemetry vocabulary.
pub(crate) fn set_http_method(span: &Span, method: &Method) {
    span.set_attribute(
        HTTP_REQUEST_METHOD_KEY,
        HttpMethod::from_method(method).as_str(),
    );
}

/// Records the matched route template as an OpenTelemetry span attribute.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `matched_path` - The route template captured by Axum, or `None` for unmatched routes.
///
/// This helper intentionally accepts only Axum's [`MatchedPath`] or `None`; callers cannot pass
/// raw request paths, query strings, or arbitrary identifiers into the route attribute.
pub(crate) fn set_http_route(span: &Span, matched_path: Option<&MatchedPath>) {
    span.set_attribute(
        HTTP_ROUTE_KEY,
        matched_path
            .map_or("unmatched", MatchedPath::as_str)
            .to_owned(),
    );
}

/// Records the HTTP status code and bounded response class as OpenTelemetry span attributes.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `status` - The HTTP response status to classify.
pub(crate) fn set_http_response(span: &Span, status: StatusCode) {
    span.set_attribute(HTTP_RESPONSE_STATUS_CODE_KEY, i64::from(status.as_u16()));
    span.set_attribute(HTTP_RESULT_KEY, HttpResult::from_status(status).as_str());
    set_status(span, http_operation_outcome(status));
}

/// Records the bounded repository-configuration operation as an OpenTelemetry span attribute.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `operation` - The fixed configuration operation vocabulary value.
pub(crate) fn set_config_operation(span: &Span, operation: ConfigOperation) {
    span.set_attribute(CONFIG_OPERATION_KEY, operation.as_str());
}

/// Records the normalized webhook event type and action as OpenTelemetry span attributes.
///
/// # Parameters
///
/// * `span` - The authenticated webhook processing span.
/// * `event_type` - The event type from the fixed metrics vocabulary.
/// * `action` - The action from the fixed metrics vocabulary.
pub(crate) fn set_webhook_event(span: &Span, event_type: EventType, action: Action) {
    span.set_attribute(WEBHOOK_EVENT_TYPE_KEY, event_type.as_str());
    span.set_attribute(WEBHOOK_ACTION_KEY, action.as_str());
}

/// Records the bounded entity represented by a merge-queue update.
///
/// # Parameters
///
/// * `span` - The merge-queue update span.
/// * `entity` - The fixed entity vocabulary value.
pub(crate) fn set_queue_entity(span: &Span, entity: QueueEntity) {
    span.set_attribute(QUEUE_ENTITY_KEY, entity.as_str());
}

/// Records a normalized merge-group transition without accepting raw payload values.
///
/// # Parameters
///
/// * `span` - The merge-queue update span.
/// * `action` - The bounded merge-group action.
/// * `reason` - The bounded merge-group reason.
pub(crate) fn set_merge_group_transition(
    span: &Span,
    action: MergeGroupAction,
    reason: MergeGroupReason,
) {
    span.set_attribute(MERGE_GROUP_ACTION_KEY, action.as_str());
    span.set_attribute(MERGE_GROUP_REASON_KEY, reason.as_str());
}

/// Records an invariant merge-queue completion outcome and reason pairing.
///
/// # Parameters
///
/// * `span` - The merge-queue update span.
/// * `completion` - The evidence-backed bounded completion classification.
pub(crate) fn set_merge_queue_completion(span: &Span, completion: MergeQueueCompletion) {
    let (outcome, reason) = match completion {
        MergeQueueCompletion::PullRequestMerged => (
            MergeQueueOutcome::Succeeded,
            MergeQueueReason::PullRequestMerged,
        ),
        MergeQueueCompletion::UnclassifiedDequeue => (
            MergeQueueOutcome::Unknown,
            MergeQueueReason::UnclassifiedDequeue,
        ),
    };
    span.set_attribute(QUEUE_OUTCOME_KEY, outcome.as_str());
    span.set_attribute(QUEUE_REASON_KEY, reason.as_str());
}

/// Records the canonical repository name as an OpenTelemetry span attribute.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `name` - The validated canonical repository name.
pub(crate) fn set_repository_name(span: &Span, name: &CanonicalRepositoryName) {
    set_key_value(span, repository_name_attribute(name));
}

/// Records the repository database identifier as an OpenTelemetry span attribute.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `id` - The validated positive repository identifier.
pub(crate) fn set_repository_id(span: &Span, id: RepositoryId) {
    span.set_attribute(REPOSITORY_ID_KEY, id.get());
}

/// Records the GitHub delivery identifier as an OpenTelemetry span attribute.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `id` - The validated delivery UUID.
pub(crate) fn set_delivery_id(span: &Span, id: &DeliveryId) {
    set_key_value(span, delivery_id_attribute(id));
}

/// Records the pull-request number as an OpenTelemetry span attribute.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `number` - The validated positive pull-request number.
pub(crate) fn set_pull_request_number(span: &Span, number: PullRequestNumber) {
    set_key_value(span, pull_request_number_attribute(number));
}

/// Records the Git commit SHA as an OpenTelemetry span attribute.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `sha` - The validated commit SHA.
pub(crate) fn set_commit_sha(span: &Span, sha: &CommitSha) {
    set_key_value(span, commit_sha_attribute(sha));
}

/// Records the bounded terminal outcome and maps failure to an error status.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `outcome` - The bounded operation outcome.
pub(crate) fn set_status(span: &Span, outcome: OperationOutcome) {
    set_key_value(span, operation_outcome_attribute(outcome));
    span.set_status(match outcome {
        OperationOutcome::Failure => Status::error("operation_failed"),
        OperationOutcome::Success
        | OperationOutcome::Duplicate
        | OperationOutcome::NoOp
        | OperationOutcome::Cancelled => Status::Ok,
    });
}

/// Records a success or failure outcome from a typed [`Result`].
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `result` - The operation result to classify without recording error details.
pub(crate) fn set_result_status<T, E>(span: &Span, result: &Result<T, E>) {
    let outcome = if result.is_ok() {
        OperationOutcome::Success
    } else {
        OperationOutcome::Failure
    };
    set_status(span, outcome);
}

/// Adds a bounded failure event to the active span.
///
/// # Parameters
///
/// * `span` - The active tracing span.
/// * `reason` - A fixed failure reason from the bounded telemetry vocabulary.
pub(crate) fn add_failure_event(span: &Span, reason: OperationFailureReason) {
    span.add_event(
        OPERATION_FAILURE_EVENT,
        vec![KeyValue::new(FAILURE_REASON_KEY, reason.as_str())],
    );
}

#[cfg(test)]
impl Operation {
    /// Returns the fixed span name for this operation.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::HttpRequest => "http.request",
            Self::WebhookAuthenticate => "github.webhook.authenticate",
            Self::WebhookProcess => "github.webhook.process",
            Self::RepositoryWrite => "config.repository.write",
            Self::SqliteQuery => "sqlite.query",
            Self::MergeQueueUpdate => "merge_queue.update",
            Self::RetentionRun => "retention.run",
        }
    }
}

impl QueueEntity {
    /// Returns the fixed queue-entity value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::MergeGroup => "merge_group",
            Self::PullRequest => "pull_request",
        }
    }
}

impl OperationFailureReason {
    /// Returns the fixed operation-failure reason.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::QueueState => "queue_state",
        }
    }
}

impl OperationOutcome {
    /// Returns the fixed span outcome value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Duplicate => "duplicate",
            Self::NoOp => "no_op",
            Self::Cancelled => "cancelled",
            Self::Failure => "failure",
        }
    }
}

impl HttpMethod {
    /// Maps an HTTP method to the fixed telemetry vocabulary.
    pub(crate) fn from_method(method: &Method) -> Self {
        match method.as_str() {
            "GET" => Self::Get,
            "POST" => Self::Post,
            "PATCH" => Self::Patch,
            "DELETE" => Self::Delete,
            "HEAD" => Self::Head,
            "OPTIONS" => Self::Options,
            "PUT" => Self::Put,
            "TRACE" | "CONNECT" | "" => Self::Other,
            _custom_method => Self::Other,
        }
    }

    /// Returns the fixed HTTP method value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Head => "HEAD",
            Self::Options => "OPTIONS",
            Self::Put => "PUT",
            Self::Other => "other",
        }
    }
}

impl HttpResult {
    /// Maps an HTTP status code to the fixed telemetry response class.
    pub(crate) const fn from_status(status: StatusCode) -> Self {
        match status.as_u16() {
            100..=399 => Self::Success,
            400..=499 => Self::ClientError,
            500..=599 => Self::ServerError,
            0..=99 | 600..=u16::MAX => Self::Other,
        }
    }

    /// Returns the fixed HTTP response class value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::ClientError => "client_error",
            Self::ServerError => "server_error",
            Self::Other => "other",
        }
    }
}

impl ConfigOperation {
    /// Returns the fixed repository-configuration operation value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Create => "create",
            Self::Update => "update",
            Self::Delete => "delete",
        }
    }
}

const fn http_operation_outcome(status: StatusCode) -> OperationOutcome {
    match status.as_u16() {
        100..=399 => OperationOutcome::Success,
        400..=u16::MAX => OperationOutcome::Failure,
        0..=99 => OperationOutcome::Failure,
    }
}

impl DatabaseOperation {
    /// Returns the fixed database-operation name.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::RepositoryCount => "repository.count",
            Self::RepositoryCreate => "repository.create",
            Self::RepositoryList => "repository.list",
            Self::RepositoryAuthenticate => "repository.authenticate",
            Self::RepositoryGet => "repository.get",
            Self::RepositoryUpdate => "repository.update",
            Self::RepositoryDelete => "repository.delete",
            Self::DeliveryClaim => "delivery.claim",
            Self::DeliveryPrune => "delivery.prune",
            Self::MergeQueueEnqueue => "merge_queue.enqueue",
            Self::MergeQueueComplete => "merge_queue.complete",
            Self::MergeQueuePrune => "merge_queue.prune",
            Self::WorkflowRunUpsert => "workflow_run.upsert",
            Self::WorkflowRunGet => "workflow_run.get",
            Self::WorkflowRunPrune => "workflow_run.prune",
            Self::WorkflowJobLinkRecord => "workflow_job_link.record",
            Self::WorkflowJobLinkList => "workflow_job_link.list",
            Self::WorkflowJobLinkPrune => "workflow_job_link.prune",
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex};

    use opentelemetry::trace::{Status, TracerProvider as _};
    use opentelemetry_sdk::error::OTelSdkResult;
    use opentelemetry_sdk::trace::{
        SdkTracerProvider, SimpleSpanProcessor, SpanData, SpanExporter,
    };
    use tracing::Dispatch;
    use tracing_subscriber::{fmt::format::FmtSpan, layer::SubscriberExt, registry::Registry};

    use crate::domain::{
        delivery::DeliveryId, merge_queue::PullRequestNumber, repository::RepositoryId,
    };
    use crate::metrics::{
        Action, EventType, MergeGroupReason, MergeQueueOutcome, MergeQueueReason,
    };
    use crate::security::CanonicalRepositoryName;

    use super::{
        add_failure_event, commit_sha_attribute, database_span, delivery_id_attribute,
        operation_outcome_attribute, operation_span, pull_request_number_attribute,
        pull_request_numbers_attribute, repository_name_attribute, set_commit_sha,
        set_config_operation, set_delivery_id, set_http_method, set_http_response, set_http_route,
        set_pull_request_number, set_repository_id, set_repository_name, set_result_status,
        set_status, timing_source_attribute, workflow_conclusion_attribute,
        workflow_error_type_attribute, workflow_head_revision_attribute, workflow_name_attribute,
        workflow_pipeline_run_id_attribute, workflow_pipeline_run_url_attribute,
        workflow_pipeline_step_task_run_id_attribute, workflow_pipeline_task_run_id_attribute,
        workflow_pipeline_task_run_result_attribute, workflow_repository_name_attribute,
        workflow_repository_url_attribute, workflow_run_attempt_attribute,
        workflow_task_name_attribute, workflow_task_run_url_attribute, CommitSha, ConfigOperation,
        DatabaseOperation, HttpMethod, HttpResult, Operation, OperationFailureReason,
        OperationOutcome, QueueEntity, TimingSource, WorkflowConclusion, WorkflowJobId,
        WorkflowRunAttempt, WorkflowRunId, CICD_PIPELINE_NAME_KEY, CICD_PIPELINE_RUN_ID_KEY,
        CICD_PIPELINE_RUN_URL_KEY, CICD_PIPELINE_TASK_NAME_KEY, CICD_PIPELINE_TASK_RUN_ID_KEY,
        CICD_PIPELINE_TASK_RUN_RESULT_KEY, CICD_PIPELINE_TASK_RUN_URL_KEY, COMMIT_SHA_KEY,
        CONFIG_OPERATION_KEY, DB_OPERATION_NAME_KEY, DB_SYSTEM_NAME_KEY, DELIVERY_ID_KEY,
        ERROR_TYPE_KEY, FAILURE_REASON_KEY, GITHUB_WORKFLOW_CONCLUSION_KEY,
        GITHUB_WORKFLOW_RUN_ATTEMPT_KEY, HTTP_REQUEST_METHOD_KEY, HTTP_RESPONSE_STATUS_CODE_KEY,
        HTTP_RESULT_KEY, HTTP_ROUTE_KEY, OPERATION_FAILURE_EVENT, OPERATION_OUTCOME_KEY,
        PULL_REQUEST_NUMBER_KEY, REPOSITORY_ID_KEY, REPOSITORY_NAME_KEY, SQLITE_SYSTEM_NAME,
        TIMING_SOURCE_KEY, VCS_HEAD_REVISION_KEY, VCS_REPOSITORY_NAME_KEY, VCS_REPOSITORY_URL_KEY,
    };

    const TEST_DELIVERY_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
    const TEST_COMMIT_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
    const TEST_REPOSITORY_NAME: &str = "Owner/Private-Repository";

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

    #[derive(Clone, Default)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl SharedWriter {
        fn contents(&self) -> String {
            let bytes = self.0.lock().expect("capture lock is available").clone();
            String::from_utf8(bytes).expect("tracing output is UTF-8")
        }
    }

    impl Write for SharedWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .expect("capture lock is available")
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for SharedWriter {
        type Writer = Self;

        fn make_writer(&'writer self) -> Self::Writer {
            self.clone()
        }
    }

    fn test_subscriber(
        writer: SharedWriter,
    ) -> (Dispatch, CollectingSpanExporter, SdkTracerProvider) {
        let exporter = CollectingSpanExporter::default();
        let provider = SdkTracerProvider::builder()
            .with_span_processor(SimpleSpanProcessor::new(exporter.clone()))
            .build();
        let tracer = provider.tracer("github_webhook_exporter");
        let otel_layer = tracing_opentelemetry::layer().with_tracer(tracer);
        let subscriber = Registry::default()
            .with(
                tracing_subscriber::fmt::layer()
                    .with_ansi(false)
                    .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
                    .with_writer(writer),
            )
            .with(otel_layer);
        (Dispatch::new(subscriber), exporter, provider)
    }

    #[test]
    fn bounded_values_remain_fixed_for_every_variant() {
        let operations = [
            (Operation::HttpRequest, "http.request"),
            (
                Operation::WebhookAuthenticate,
                "github.webhook.authenticate",
            ),
            (Operation::WebhookProcess, "github.webhook.process"),
            (Operation::RepositoryWrite, "config.repository.write"),
            (Operation::SqliteQuery, "sqlite.query"),
            (Operation::MergeQueueUpdate, "merge_queue.update"),
            (Operation::RetentionRun, "retention.run"),
        ];
        for (operation, expected) in operations {
            assert_eq!(operation.as_str(), expected);
        }

        let outcomes = [
            (OperationOutcome::Success, "success"),
            (OperationOutcome::Duplicate, "duplicate"),
            (OperationOutcome::NoOp, "no_op"),
            (OperationOutcome::Cancelled, "cancelled"),
            (OperationOutcome::Failure, "failure"),
        ];
        for (outcome, expected) in outcomes {
            assert_eq!(outcome.as_str(), expected);
        }

        let http_methods = [
            (HttpMethod::Get, "GET"),
            (HttpMethod::Post, "POST"),
            (HttpMethod::Patch, "PATCH"),
            (HttpMethod::Delete, "DELETE"),
            (HttpMethod::Head, "HEAD"),
            (HttpMethod::Options, "OPTIONS"),
            (HttpMethod::Put, "PUT"),
            (HttpMethod::Other, "other"),
        ];
        for (method, expected) in http_methods {
            assert_eq!(method.as_str(), expected);
        }

        let http_results = [
            (HttpResult::Success, "success"),
            (HttpResult::ClientError, "client_error"),
            (HttpResult::ServerError, "server_error"),
            (HttpResult::Other, "other"),
        ];
        for (result, expected) in http_results {
            assert_eq!(result.as_str(), expected);
        }

        let config_operations = [
            (ConfigOperation::Create, "create"),
            (ConfigOperation::Update, "update"),
            (ConfigOperation::Delete, "delete"),
        ];
        for (operation, expected) in config_operations {
            assert_eq!(operation.as_str(), expected);
        }

        let queue_entities = [
            (QueueEntity::MergeGroup, "merge_group"),
            (QueueEntity::PullRequest, "pull_request"),
        ];
        for (entity, expected) in queue_entities {
            assert_eq!(entity.as_str(), expected);
        }

        let failure_reasons = [(OperationFailureReason::QueueState, "queue_state")];
        for (reason, expected) in failure_reasons {
            assert_eq!(reason.as_str(), expected);
        }

        let database_operations = [
            (DatabaseOperation::RepositoryCount, "repository.count"),
            (DatabaseOperation::RepositoryCreate, "repository.create"),
            (DatabaseOperation::RepositoryList, "repository.list"),
            (
                DatabaseOperation::RepositoryAuthenticate,
                "repository.authenticate",
            ),
            (DatabaseOperation::RepositoryGet, "repository.get"),
            (DatabaseOperation::RepositoryUpdate, "repository.update"),
            (DatabaseOperation::RepositoryDelete, "repository.delete"),
            (DatabaseOperation::DeliveryClaim, "delivery.claim"),
            (DatabaseOperation::DeliveryPrune, "delivery.prune"),
            (DatabaseOperation::MergeQueueEnqueue, "merge_queue.enqueue"),
            (
                DatabaseOperation::MergeQueueComplete,
                "merge_queue.complete",
            ),
            (DatabaseOperation::MergeQueuePrune, "merge_queue.prune"),
            (DatabaseOperation::WorkflowRunUpsert, "workflow_run.upsert"),
            (DatabaseOperation::WorkflowRunGet, "workflow_run.get"),
            (DatabaseOperation::WorkflowRunPrune, "workflow_run.prune"),
            (
                DatabaseOperation::WorkflowJobLinkRecord,
                "workflow_job_link.record",
            ),
            (
                DatabaseOperation::WorkflowJobLinkList,
                "workflow_job_link.list",
            ),
            (
                DatabaseOperation::WorkflowJobLinkPrune,
                "workflow_job_link.prune",
            ),
        ];
        for (operation, expected) in database_operations {
            assert_eq!(operation.as_str(), expected);
        }
    }

    #[test]
    fn http_and_config_helpers_export_only_bounded_values() {
        let output = SharedWriter::default();
        let (dispatch, exporter, provider) = test_subscriber(output);

        tracing::dispatcher::with_default(&dispatch, || {
            let success_span = operation_span(Operation::HttpRequest);
            set_http_method(&success_span, &http::Method::POST);
            set_http_route(&success_span, None);
            set_http_response(&success_span, http::StatusCode::CREATED);
            drop(success_span);

            let client_error_span = operation_span(Operation::HttpRequest);
            set_http_method(
                &client_error_span,
                &http::Method::from_bytes(b"BREW").expect("custom method is valid"),
            );
            set_http_route(&client_error_span, None);
            set_http_response(&client_error_span, http::StatusCode::BAD_REQUEST);
            drop(client_error_span);

            let server_error_span = operation_span(Operation::HttpRequest);
            set_http_method(&server_error_span, &http::Method::GET);
            set_http_route(&server_error_span, None);
            set_http_response(&server_error_span, http::StatusCode::INTERNAL_SERVER_ERROR);
            drop(server_error_span);

            let other_span = operation_span(Operation::HttpRequest);
            set_http_method(&other_span, &http::Method::TRACE);
            set_http_route(&other_span, None);
            set_http_response(
                &other_span,
                http::StatusCode::from_u16(700).expect("700 is an extension status code"),
            );
            drop(other_span);

            let repository_span = operation_span(Operation::RepositoryWrite);
            set_config_operation(&repository_span, ConfigOperation::Update);
            let result: Result<(), ()> = Err(());
            set_result_status(&repository_span, &result);
            drop(repository_span);
        });

        provider.force_flush().expect("spans flush");
        let spans = exporter.finished_spans();

        assert!(spans.iter().any(|span| {
            span.name.as_ref() == "http.request"
                && span.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == HTTP_REQUEST_METHOD_KEY
                        && attribute.value.as_str().as_ref() == "POST"
                })
                && span.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == HTTP_ROUTE_KEY
                        && attribute.value.as_str().as_ref() == "unmatched"
                })
                && span.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == HTTP_RESPONSE_STATUS_CODE_KEY
                        && attribute.value.as_str().as_ref() == "201"
                })
                && span.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == HTTP_RESULT_KEY
                        && attribute.value.as_str().as_ref() == HttpResult::Success.as_str()
                })
        }));
        assert!(spans.iter().any(|span| {
            span.name.as_ref() == "http.request"
                && span.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == HTTP_REQUEST_METHOD_KEY
                        && attribute.value.as_str().as_ref() == HttpMethod::Other.as_str()
                })
                && span.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == HTTP_RESULT_KEY
                        && attribute.value.as_str().as_ref() == HttpResult::ClientError.as_str()
                })
        }));
        assert!(spans.iter().any(|span| {
            span.name.as_ref() == "http.request"
                && span.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == HTTP_RESULT_KEY
                        && attribute.value.as_str().as_ref() == HttpResult::ServerError.as_str()
                })
        }));
        assert!(spans.iter().any(|span| {
            span.name.as_ref() == "http.request"
                && span.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == HTTP_RESULT_KEY
                        && attribute.value.as_str().as_ref() == HttpResult::Other.as_str()
                })
        }));
        assert!(spans.iter().any(|span| {
            span.name.as_ref() == "config.repository.write"
                && span.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == CONFIG_OPERATION_KEY
                        && attribute.value.as_str().as_ref() == ConfigOperation::Update.as_str()
                })
                && span.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == OPERATION_OUTCOME_KEY
                        && attribute.value.as_str().as_ref() == OperationOutcome::Failure.as_str()
                })
        }));
    }

    #[test]
    fn commit_sha_parser_rejects_invalid_values_without_echoing_input() {
        let commit_sha = CommitSha::parse("0123456789ABCDEF0123456789abcdef01234567")
            .expect("mixed-case SHA-1 commit identifier is valid");
        assert_eq!(
            commit_sha.as_str(),
            "0123456789abcdef0123456789abcdef01234567"
        );
        let sha_256 =
            CommitSha::parse("ABCDEF0123456789abcdef0123456789ABCDEF0123456789abcdef0123456789")
                .expect("mixed-case SHA-256 commit identifier is valid");
        assert_eq!(
            sha_256.as_str(),
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );

        for invalid in [
            "not-a-sha",
            "0123456789abcdef0123456789abcdef0123456",
            "g123456789abcdef0123456789abcdef0123456",
        ] {
            let error = CommitSha::parse(invalid).expect_err("invalid commit SHA is rejected");
            let rendered = format!("{error}");
            assert!(!rendered.contains(invalid));
            assert_eq!(
                rendered,
                "commit SHA is not a valid 40- or 64-character hexadecimal string"
            );
        }

        let error = CommitSha::parse("").expect_err("empty commit SHA is rejected");
        assert_eq!(
            error.to_string(),
            "commit SHA is not a valid 40- or 64-character hexadecimal string"
        );
    }

    #[test]
    fn operation_spans_keep_sensitive_identifiers_out_of_fmt_output_and_export_otlp_attributes() {
        let repository_name = CanonicalRepositoryName::new(TEST_REPOSITORY_NAME)
            .expect("test repository name is valid");
        let delivery_id = DeliveryId::parse(TEST_DELIVERY_ID).expect("test delivery UUID is valid");
        let repository_id = RepositoryId::new(42).expect("test repository id is positive");
        let pull_request_number =
            PullRequestNumber::new(17).expect("test pull request number is positive");
        let commit_sha = CommitSha::parse(TEST_COMMIT_SHA).expect("test commit SHA is valid");
        let output = SharedWriter::default();
        let (dispatch, exporter, provider) = test_subscriber(output.clone());

        tracing::dispatcher::with_default(&dispatch, || {
            let request_span = operation_span(Operation::WebhookProcess);
            set_repository_name(&request_span, &repository_name);
            set_repository_id(&request_span, repository_id);
            set_delivery_id(&request_span, &delivery_id);
            set_pull_request_number(&request_span, pull_request_number);
            set_commit_sha(&request_span, &commit_sha);
            set_status(&request_span, OperationOutcome::Failure);
            add_failure_event(&request_span, OperationFailureReason::QueueState);
            drop(request_span);

            let database = database_span(DatabaseOperation::RepositoryCreate);
            drop(database);
        });

        provider.force_flush().expect("spans flush");
        let spans = exporter.finished_spans();
        assert_eq!(spans.len(), 2);

        let request_span = spans
            .iter()
            .find(|span| span.name.as_ref() == "github.webhook.process")
            .expect("request span is exported");
        let database_span = spans
            .iter()
            .find(|span| span.name.as_ref() == "sqlite.query")
            .expect("database span is exported");

        assert!(request_span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == REPOSITORY_NAME_KEY
                && attribute.value.as_str().as_ref() == repository_name.as_str()
        }));
        assert!(request_span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == REPOSITORY_ID_KEY && attribute.value.as_str().as_ref() == "42"
        }));
        assert!(request_span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == DELIVERY_ID_KEY
                && attribute.value.as_str().as_ref() == TEST_DELIVERY_ID
        }));
        assert!(request_span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == PULL_REQUEST_NUMBER_KEY
                && matches!(attribute.value, opentelemetry::Value::I64(17))
        }));
        assert!(request_span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == COMMIT_SHA_KEY
                && attribute.value.as_str().as_ref() == TEST_COMMIT_SHA
        }));
        assert!(request_span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == OPERATION_OUTCOME_KEY
                && attribute.value.as_str().as_ref() == OperationOutcome::Failure.as_str()
        }));
        assert!(request_span.events.events.iter().any(|event| {
            event.name.as_ref() == OPERATION_FAILURE_EVENT
                && event.attributes.iter().any(|attribute| {
                    attribute.key.as_str() == FAILURE_REASON_KEY
                        && attribute.value.as_str().as_ref() == "queue_state"
                })
        }));
        assert!(
            matches!(request_span.status, Status::Error { ref description } if description.as_ref() == "operation_failed")
        );

        assert!(database_span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == DB_SYSTEM_NAME_KEY
                && attribute.value.as_str().as_ref() == SQLITE_SYSTEM_NAME
        }));
        assert!(database_span.attributes.iter().any(|attribute| {
            attribute.key.as_str() == DB_OPERATION_NAME_KEY
                && attribute.value.as_str().as_ref() == DatabaseOperation::RepositoryCreate.as_str()
        }));

        let stderr = output.contents();
        assert!(!stderr.contains("owner/private-repository"));
        assert!(!stderr.contains(TEST_DELIVERY_ID));
        assert!(!stderr.contains(TEST_COMMIT_SHA));
    }

    #[test]
    fn workflow_attribute_builders_export_bounded_values() {
        let repository_name = CanonicalRepositoryName::new(TEST_REPOSITORY_NAME)
            .expect("test repository name is valid");
        let delivery_id = DeliveryId::parse(TEST_DELIVERY_ID).expect("test delivery UUID is valid");
        let commit_sha = CommitSha::parse(TEST_COMMIT_SHA).expect("test commit SHA is valid");
        let workflow_name = crate::telemetry::workflow::DisplayName::sanitize("Build\nWorkflow")
            .expect("workflow name sanitizes");
        let job_name = crate::telemetry::workflow::DisplayName::sanitize("Linux\tJob")
            .expect("job name sanitizes");
        let run_id = WorkflowRunId::new(31).expect("workflow run id is positive");
        let run_attempt = WorkflowRunAttempt::new(2).expect("workflow run attempt is positive");
        let job_id = WorkflowJobId::new(41).expect("workflow job id is positive");
        let pull_request_number =
            PullRequestNumber::new(17).expect("test pull request number is positive");

        let workflow_name_kv = workflow_name_attribute(workflow_name.as_str());
        assert_eq!(workflow_name_kv.key.as_str(), CICD_PIPELINE_NAME_KEY);
        assert_eq!(workflow_name_kv.value.as_str().as_ref(), "BuildWorkflow");

        let workflow_task_name_kv = workflow_task_name_attribute(job_name.as_str());
        assert_eq!(
            workflow_task_name_kv.key.as_str(),
            CICD_PIPELINE_TASK_NAME_KEY
        );
        assert_eq!(workflow_task_name_kv.value.as_str().as_ref(), "LinuxJob");

        let pipeline_run_id_kv = workflow_pipeline_run_id_attribute(run_id);
        assert_eq!(pipeline_run_id_kv.key.as_str(), CICD_PIPELINE_RUN_ID_KEY);
        assert_eq!(pipeline_run_id_kv.value.as_str().as_ref(), "31");

        let attempt_kv = workflow_run_attempt_attribute(run_attempt);
        assert_eq!(attempt_kv.key.as_str(), GITHUB_WORKFLOW_RUN_ATTEMPT_KEY);
        assert_eq!(attempt_kv.value.as_str().as_ref(), "2");

        let job_task_run_id_kv = workflow_pipeline_task_run_id_attribute(job_id);
        assert_eq!(
            job_task_run_id_kv.key.as_str(),
            CICD_PIPELINE_TASK_RUN_ID_KEY
        );
        assert_eq!(job_task_run_id_kv.value.as_str().as_ref(), "41");

        let step = crate::telemetry::workflow::WorkflowStepTrace::new(
            3,
            None,
            WorkflowConclusion::Success,
            crate::telemetry::workflow::HistoricalTiming::fallback(
                std::time::SystemTime::UNIX_EPOCH,
            ),
        )
        .expect("workflow step number is positive");
        let step_task_run_id_kv = workflow_pipeline_step_task_run_id_attribute(job_id, &step);
        assert_eq!(
            step_task_run_id_kv.key.as_str(),
            CICD_PIPELINE_TASK_RUN_ID_KEY
        );
        assert_eq!(step_task_run_id_kv.value.as_str().as_ref(), "41:3");

        let conclusion_kv = workflow_conclusion_attribute(WorkflowConclusion::Cancelled);
        assert_eq!(conclusion_kv.key.as_str(), GITHUB_WORKFLOW_CONCLUSION_KEY);
        assert_eq!(conclusion_kv.value.as_str().as_ref(), "cancelled");

        let task_result_kv =
            workflow_pipeline_task_run_result_attribute(WorkflowConclusion::TimedOut);
        assert_eq!(
            task_result_kv.key.as_str(),
            CICD_PIPELINE_TASK_RUN_RESULT_KEY
        );
        assert_eq!(task_result_kv.value.as_str().as_ref(), "timeout");
        assert_eq!(
            workflow_pipeline_task_run_result_attribute(WorkflowConclusion::Neutral)
                .value
                .as_str()
                .as_ref(),
            "neutral"
        );
        assert_eq!(
            workflow_pipeline_task_run_result_attribute(WorkflowConclusion::Other)
                .value
                .as_str()
                .as_ref(),
            "other"
        );

        let run_url = workflow_pipeline_run_url_attribute(&repository_name, run_id);
        assert_eq!(run_url.key.as_str(), CICD_PIPELINE_RUN_URL_KEY);
        assert_eq!(
            run_url.value.as_str().as_ref(),
            "https://github.com/owner/private-repository/actions/runs/31"
        );
        let task_url = workflow_task_run_url_attribute(&repository_name, run_id, job_id);
        assert_eq!(task_url.key.as_str(), CICD_PIPELINE_TASK_RUN_URL_KEY);
        assert_eq!(
            task_url.value.as_str().as_ref(),
            "https://github.com/owner/private-repository/actions/runs/31/job/41"
        );
        assert_eq!(
            workflow_repository_url_attribute(&repository_name)
                .key
                .as_str(),
            VCS_REPOSITORY_URL_KEY
        );
        let vcs_repository_name = workflow_repository_name_attribute(&repository_name);
        assert_eq!(vcs_repository_name.key.as_str(), VCS_REPOSITORY_NAME_KEY);
        assert_eq!(
            vcs_repository_name.value.as_str().as_ref(),
            "private-repository"
        );
        assert_eq!(
            workflow_head_revision_attribute(&commit_sha).key.as_str(),
            VCS_HEAD_REVISION_KEY
        );
        assert_eq!(
            workflow_error_type_attribute(WorkflowConclusion::Failure)
                .expect("failure has an error type")
                .key
                .as_str(),
            ERROR_TYPE_KEY
        );
        assert!(workflow_error_type_attribute(WorkflowConclusion::Success).is_none());

        let timing_source_kv = timing_source_attribute(TimingSource::Fallback);
        assert_eq!(timing_source_kv.key.as_str(), TIMING_SOURCE_KEY);
        assert_eq!(timing_source_kv.value.as_str().as_ref(), "fallback");

        let repository_name_kv = repository_name_attribute(&repository_name);
        assert_eq!(repository_name_kv.key.as_str(), REPOSITORY_NAME_KEY);
        assert_eq!(
            repository_name_kv.value.as_str().as_ref(),
            repository_name.as_str()
        );

        let delivery_id_kv = delivery_id_attribute(&delivery_id);
        assert_eq!(delivery_id_kv.key.as_str(), DELIVERY_ID_KEY);
        assert_eq!(delivery_id_kv.value.as_str().as_ref(), TEST_DELIVERY_ID);

        let pull_request_number_kv = pull_request_number_attribute(pull_request_number);
        assert_eq!(pull_request_number_kv.key.as_str(), PULL_REQUEST_NUMBER_KEY);
        assert_eq!(pull_request_number_kv.value, opentelemetry::Value::I64(17));

        let pull_request_numbers_kv = pull_request_numbers_attribute(&[
            pull_request_number,
            PullRequestNumber::new(23).expect("test pull request number is positive"),
        ])
        .expect("bounded pull-request list is present");
        assert_eq!(
            pull_request_numbers_kv.key.as_str(),
            PULL_REQUEST_NUMBER_KEY
        );
        assert_eq!(
            pull_request_numbers_kv.value,
            opentelemetry::Value::Array(opentelemetry::Array::I64(vec![17, 23]))
        );
        assert!(pull_request_numbers_attribute(&[]).is_none());

        let commit_sha_kv = commit_sha_attribute(&commit_sha);
        assert_eq!(commit_sha_kv.key.as_str(), COMMIT_SHA_KEY);
        assert_eq!(commit_sha_kv.value.as_str().as_ref(), TEST_COMMIT_SHA);

        let outcome_kv = operation_outcome_attribute(OperationOutcome::Failure);
        assert_eq!(outcome_kv.key.as_str(), OPERATION_OUTCOME_KEY);
        assert_eq!(outcome_kv.value.as_str().as_ref(), "failure");
    }

    #[test]
    fn bounded_metrics_enums_remain_closed_to_untrusted_input() {
        for (event_type, expected) in [
            (EventType::BranchProtectionRule, "branch_protection_rule"),
            (EventType::CheckRun, "check_run"),
            (EventType::CheckSuite, "check_suite"),
            (EventType::Create, "create"),
            (EventType::Delete, "delete"),
            (EventType::Deployment, "deployment"),
            (EventType::DeploymentStatus, "deployment_status"),
            (EventType::Discussion, "discussion"),
            (EventType::DiscussionComment, "discussion_comment"),
            (EventType::Issues, "issues"),
            (EventType::IssueComment, "issue_comment"),
            (EventType::MergeGroup, "merge_group"),
            (EventType::PullRequest, "pull_request"),
            (EventType::PullRequestReview, "pull_request_review"),
            (
                EventType::PullRequestReviewComment,
                "pull_request_review_comment",
            ),
            (EventType::Push, "push"),
            (EventType::Release, "release"),
            (EventType::Repository, "repository"),
            (EventType::Status, "status"),
            (EventType::WorkflowJob, "workflow_job"),
            (EventType::WorkflowRun, "workflow_run"),
            (EventType::Other, "other"),
        ] {
            assert_eq!(event_type.as_str(), expected);
        }

        for (action, expected) in [
            (Action::Assigned, "assigned"),
            (Action::ChecksRequested, "checks_requested"),
            (Action::Closed, "closed"),
            (Action::Completed, "completed"),
            (Action::Created, "created"),
            (Action::Deleted, "deleted"),
            (Action::Dequeued, "dequeued"),
            (Action::Destroyed, "destroyed"),
            (Action::Edited, "edited"),
            (Action::Enqueued, "enqueued"),
            (Action::InProgress, "in_progress"),
            (Action::Labeled, "labeled"),
            (Action::Opened, "opened"),
            (Action::Published, "published"),
            (Action::Queued, "queued"),
            (Action::Reopened, "reopened"),
            (Action::Requested, "requested"),
            (Action::RequestedAction, "requested_action"),
            (Action::Rerequested, "rerequested"),
            (Action::Submitted, "submitted"),
            (Action::Synchronize, "synchronize"),
            (Action::Unassigned, "unassigned"),
            (Action::Unlabeled, "unlabeled"),
            (Action::Unpublished, "unpublished"),
            (Action::Updated, "updated"),
            (Action::Waiting, "waiting"),
            (Action::None, "none"),
            (Action::Other, "other"),
        ] {
            assert_eq!(action.as_str(), expected);
        }

        for (reason, expected) in [
            (MergeGroupReason::None, "none"),
            (MergeGroupReason::Merged, "merged"),
            (MergeGroupReason::Dequeued, "dequeued"),
            (MergeGroupReason::Invalidated, "invalidated"),
            (MergeGroupReason::Other, "other"),
        ] {
            assert_eq!(reason.as_str(), expected);
        }

        for (outcome, expected) in [
            (MergeQueueOutcome::Succeeded, "succeeded"),
            (MergeQueueOutcome::Failed, "failed"),
            (MergeQueueOutcome::Cancelled, "cancelled"),
            (MergeQueueOutcome::Unknown, "unknown"),
        ] {
            assert_eq!(outcome.as_str(), expected);
        }

        for (reason, expected) in [
            (MergeQueueReason::PullRequestMerged, "pull_request_merged"),
            (
                MergeQueueReason::UnclassifiedDequeue,
                "unclassified_dequeue",
            ),
        ] {
            assert_eq!(reason.as_str(), expected);
        }
    }
}
