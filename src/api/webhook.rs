use std::time::Instant;

use axum::{
    body::Bytes,
    extract::{rejection::BytesRejection, DefaultBodyLimit, FromRequest, Request, State},
    http::{header::CONTENT_TYPE, request::Parts, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use opentelemetry::Context;
use serde::Deserialize;
use time::OffsetDateTime;
use tracing::{debug, error, warn, Instrument};

use crate::{
    api::{merge_group::EventProjection, pull_request::QueueProcessor, workflow_job, workflow_run},
    app::{AppState, RequestRepositoryContext},
    domain::{delivery::DeliveryId, repository::RepositoryId},
    error::AppError,
    metrics::{
        normalize_action, normalize_event_type, Action, EventType, FailureStage, Metrics,
        WebhookResult, WorkflowTraceRejectionReason,
    },
    security::{
        CanonicalRepositoryName, WebhookAuthenticationError, WebhookAuthenticator, WebhookSignature,
    },
    storage::DeliveryClaim,
    telemetry::{
        pipeline::{PipelineRunTrace, PipelineRunTraceParts, MAX_PIPELINE_JOB_SPANS},
        trace::{self, Operation, OperationOutcome, QueueEntity},
        workflow::WorkflowRunContext,
        LOCAL_ONLY_LOG_TARGET,
    },
};

const JSON_CONTENT_TYPE: HeaderValue = HeaderValue::from_static("application/json");

pub(super) fn router(body_limit_bytes: usize, metrics: Metrics) -> Router<AppState> {
    Router::new().route(
        "/webhooks/github",
        post(webhook_handler)
            .layer(DefaultBodyLimit::max(body_limit_bytes))
            .layer(middleware::from_fn_with_state(
                metrics,
                observe_webhook_request,
            )),
    )
}

#[derive(Deserialize)]
struct WebhookRepositoryProjection {
    repository: RepositoryProjection,
}

#[derive(Deserialize)]
struct RepositoryProjection {
    full_name: String,
}

struct WebhookRequest {
    body: Bytes,
    repository_name: CanonicalRepositoryName,
    repository_context: RequestRepositoryContext,
    event_type: String,
    delivery_id: DeliveryId,
    signature: WebhookSignature,
}

impl FromRequest<AppState> for WebhookRequest {
    type Rejection = AppError;

    async fn from_request(request: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let (parts, body) = request.into_parts();
        let repository_context = parts
            .extensions
            .get::<RequestRepositoryContext>()
            .cloned()
            .expect("HTTP middleware installs repository context");
        validate_content_type(&parts)?;
        let event_type = required_header(&parts.headers, "X-GitHub-Event")?.to_owned();
        let delivery_id = DeliveryId::parse(required_header(&parts.headers, "X-GitHub-Delivery")?)
            .map_err(|_| AppError::invalid_webhook())?;
        let signature =
            WebhookSignature::parse(required_header(&parts.headers, "X-Hub-Signature-256")?)
                .map_err(|_| AppError::invalid_webhook())?;
        let body = Bytes::from_request(Request::from_parts(parts, body), state)
            .await
            .map_err(map_body_rejection)?;
        let projection: WebhookRepositoryProjection =
            serde_json::from_slice(&body).map_err(|_| AppError::invalid_webhook())?;
        let repository_name = CanonicalRepositoryName::new(&projection.repository.full_name)
            .map_err(|_| AppError::invalid_webhook())?;

        Ok(Self {
            body,
            repository_name,
            repository_context,
            event_type,
            delivery_id,
            signature,
        })
    }
}

async fn webhook_handler(
    State(state): State<AppState>,
    request: WebhookRequest,
) -> Result<Response, AppError> {
    let received_at = OffsetDateTime::now_utc();
    let authentication_span = trace::operation_span(Operation::WebhookAuthenticate);
    trace::set_repository_name(&authentication_span, &request.repository_name);
    trace::set_delivery_id(&authentication_span, &request.delivery_id);
    let authentication = WebhookAuthenticator::new(state.repository_store())
        .authenticate(
            &request.repository_name,
            &request.signature,
            request.body.as_ref(),
        )
        .instrument(authentication_span.clone())
        .await;
    let repository_id = match authentication {
        Ok(repository_id) => {
            request
                .repository_context
                .authenticate(&request.repository_name);
            trace::set_repository_id(&authentication_span, repository_id);
            trace::set_status(&authentication_span, OperationOutcome::Success);
            repository_id
        }
        Err(WebhookAuthenticationError::Unauthorized) => {
            trace::set_status(&authentication_span, OperationOutcome::Failure);
            return Err(AppError::unauthorized_webhook());
        }
        Err(WebhookAuthenticationError::Unavailable) => {
            trace::set_status(&authentication_span, OperationOutcome::Failure);
            return Err(unavailable_error(
                &state,
                None,
                FailureStage::Authentication,
            ));
        }
    };
    drop(authentication_span);

    let process_span = trace::operation_span(Operation::WebhookProcess);
    trace::set_repository_name(&process_span, &request.repository_name);
    trace::set_repository_id(&process_span, repository_id);
    trace::set_delivery_id(&process_span, &request.delivery_id);
    let processing: Result<OperationOutcome, AppError> = async {
        let event_projection: EventProjection =
            serde_json::from_slice(&request.body).map_err(|_| AppError::invalid_webhook())?;
        let event_type = normalize_event_type(&request.event_type);
        let action = normalize_action(event_projection.action());
        trace::set_webhook_event(&process_span, event_type, action);

        let claim = state.delivery_store().claim(&request.delivery_id).await;
        match claim {
            Ok(DeliveryClaim::Duplicate) => {
                state.metrics().record_duplicate(&request.repository_name);
                Ok(OperationOutcome::Duplicate)
            }
            Ok(DeliveryClaim::New) => {
                state.metrics().observe_event(
                    &request.repository_name,
                    event_type,
                    action,
                    request.body.len(),
                );
                if event_type == EventType::WorkflowRun {
                    if let Some(context) = workflow_run::project_context(request.body.as_ref()) {
                        state
                            .workflow_run_store()
                            .upsert(repository_id, &context)
                            .await
                            .map_err(|_| {
                                unavailable_error(
                                    &state,
                                    Some(&request.repository_name),
                                    FailureStage::Database,
                                )
                            })?;
                        if action == Action::Completed {
                            emit_pipeline_trace(&state, repository_id, &request, &context).await;
                        }
                    }
                }
                if event_type == EventType::WorkflowJob && action == Action::Completed {
                    if let Some(admission) =
                        workflow_job::inspect_completed_job(request.body.as_ref())
                    {
                        state.metrics().observe_workflow_job_steps(
                            &request.repository_name,
                            admission.step_count(),
                        );
                        let step_limit = state.workflow_job_max_steps();
                        if admission.step_count() > step_limit {
                            record_workflow_trace_rejection(
                                &state,
                                &request.repository_name,
                                &request.delivery_id,
                                &admission,
                                step_limit,
                            );
                        } else {
                            let workflow_run_context = state
                                .workflow_run_store()
                                .get(repository_id, admission.run_id(), admission.run_attempt())
                                .await
                                .map_err(|_| {
                                    unavailable_error(
                                        &state,
                                        Some(&request.repository_name),
                                        FailureStage::Database,
                                    )
                                })?;
                            if let Some(workflow_trace) = workflow_job::project_completed_job(
                                request.body.as_ref(),
                                &request.repository_name,
                                &request.delivery_id,
                                received_at,
                                workflow_run_context,
                            ) {
                                if let Some(identity) =
                                    state.workflow_trace_emitter().emit(&workflow_trace)
                                {
                                    if state
                                        .workflow_job_link_store()
                                        .record(repository_id, &workflow_trace, identity)
                                        .await
                                        .is_err()
                                    {
                                        record_workflow_link_failure(
                                            &state,
                                            &request.repository_name,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some(transition) =
                    event_projection.merge_group_transition(event_type, action)
                {
                    state.metrics().record_merge_group_event(
                        &request.repository_name,
                        transition.action,
                        transition.reason,
                    );
                    let update_span = trace::operation_span(Operation::MergeQueueUpdate);
                    trace::set_queue_entity(&update_span, QueueEntity::MergeGroup);
                    trace::set_webhook_event(&update_span, event_type, action);
                    trace::set_merge_group_transition(
                        &update_span,
                        transition.action,
                        transition.reason,
                    );
                    trace::set_repository_name(&update_span, &request.repository_name);
                    trace::set_repository_id(&update_span, repository_id);
                    trace::set_delivery_id(&update_span, &request.delivery_id);
                    if let Some(head_sha) = transition.head_sha.as_ref() {
                        trace::set_commit_sha(&update_span, head_sha);
                    }
                    trace::set_status(&update_span, OperationOutcome::Success);
                }
                if let Some(pull_request) = event_projection.pull_request() {
                    let processor = QueueProcessor {
                        repository_id,
                        repository_name: &request.repository_name,
                        delivery_id: &request.delivery_id,
                        event_type,
                        action,
                        received_at,
                        store: state.merge_queue_store(),
                        metrics: state.metrics(),
                    };
                    if processor.process(pull_request).await.is_err() {
                        record_queue_state_failure(&state, &request.repository_name);
                    }
                }
                Ok(OperationOutcome::Success)
            }
            Err(_) => Err(unavailable_error(
                &state,
                Some(&request.repository_name),
                FailureStage::DeliveryClaim,
            )),
        }
    }
    .instrument(process_span.clone())
    .await;

    match processing {
        Ok(outcome) => {
            trace::set_status(&process_span, outcome);
            Ok(StatusCode::NO_CONTENT.into_response())
        }
        Err(error) => {
            trace::set_status(&process_span, OperationOutcome::Failure);
            Err(error)
        }
    }
}

/// Emits the pipeline-run summary trace for one terminal `workflow_run` delivery.
///
/// Pipeline traces are pure telemetry enrichment layered on top of already exported job traces, so
/// every failure here degrades to a bounded failure metric and leaves the authenticated
/// `204 No Content` response, readiness, and merge-queue state untouched.
///
/// # Parameters
///
/// * `state` - The shared application state.
/// * `repository_id` - The authenticated repository row identifier.
/// * `request` - The authenticated webhook request.
/// * `context` - The bounded workflow-run context projected from this delivery.
async fn emit_pipeline_trace(
    state: &AppState,
    repository_id: RepositoryId,
    request: &WebhookRequest,
    context: &WorkflowRunContext,
) {
    if !state.workflow_trace_emitter().is_enabled() {
        return;
    }
    // One extra row distinguishes an accepted run from an over-limit one without a second query.
    let jobs = match state
        .workflow_job_link_store()
        .list(
            repository_id,
            context.run_id(),
            context.run_attempt(),
            MAX_PIPELINE_JOB_SPANS + 1,
        )
        .await
    {
        Ok(jobs) => jobs,
        Err(_) => {
            record_workflow_link_failure(state, &request.repository_name);
            return;
        }
    };
    if jobs.is_empty() {
        return;
    }
    if jobs.len() > MAX_PIPELINE_JOB_SPANS {
        record_pipeline_trace_rejection(state, request, context, jobs.len());
        return;
    }

    let Some(summary) = workflow_run::project_run_summary(request.body.as_ref()) else {
        return;
    };
    let Some(pipeline) = PipelineRunTrace::new(PipelineRunTraceParts {
        repository_name: request.repository_name.clone(),
        delivery_id: request.delivery_id,
        workflow_name: summary.workflow_name,
        head_sha: summary.head_sha,
        pull_requests: summary.pull_requests,
        run_context: context.clone(),
        jobs,
    }) else {
        return;
    };
    state.workflow_trace_emitter().emit_pipeline(&pipeline);
}

fn record_pipeline_trace_rejection(
    state: &AppState,
    request: &WebhookRequest,
    context: &WorkflowRunContext,
    job_count: usize,
) {
    state.metrics().record_workflow_trace_rejection(
        &request.repository_name,
        WorkflowTraceRejectionReason::TooManyJobs,
    );
    let _parentless_context = Context::new().attach();
    let mut delivery_buffer = uuid::Uuid::encode_buffer();
    warn!(
        parent: None,
        reason = WorkflowTraceRejectionReason::TooManyJobs.as_str(),
        repository_name = request.repository_name.as_str(),
        workflow_run_id = context.run_id().get(),
        workflow_run_attempt = context.run_attempt().get(),
        delivery_id = request.delivery_id.encode_lower(&mut delivery_buffer),
        job_count,
        job_limit = MAX_PIPELINE_JOB_SPANS,
        "pipeline-run summary trace rejected"
    );
}

/// Records one bounded pipeline-link failure without changing the authenticated response.
///
/// This uses its own [`FailureStage::WorkflowLink`] rather than the generic database stage so
/// operators can alert on the health of the enrichment-only pipeline-link path directly, without
/// reading logs to separate it from the failures that do change a response.
///
/// # Parameters
///
/// * `state` - The shared application state.
/// * `repository_name` - The canonical authenticated repository name.
fn record_workflow_link_failure(state: &AppState, repository_name: &CanonicalRepositoryName) {
    state
        .metrics()
        .record_failure(Some(repository_name), FailureStage::WorkflowLink);
    let error = AppError::webhook_unavailable();
    let error_correlation_id = error
        .correlation_id()
        .expect("webhook dependency failures carry a correlation ID");
    error!(
        parent: None,
        stage = FailureStage::WorkflowLink.as_str(),
        result = WebhookResult::Unavailable.as_str(),
        %error_correlation_id,
        "workflow-job link persistence failed"
    );
}

fn record_workflow_trace_rejection(
    state: &AppState,
    repository_name: &CanonicalRepositoryName,
    delivery_id: &DeliveryId,
    admission: &workflow_job::WorkflowJobAdmission,
    step_limit: usize,
) {
    state.metrics().record_workflow_trace_rejection(
        repository_name,
        WorkflowTraceRejectionReason::TooManySteps,
    );
    let _parentless_context = Context::new().attach();
    let mut delivery_buffer = uuid::Uuid::encode_buffer();
    warn!(
        parent: None,
        reason = WorkflowTraceRejectionReason::TooManySteps.as_str(),
        repository_name = repository_name.as_str(),
        workflow_run_id = admission.run_id().get(),
        workflow_run_attempt = admission.run_attempt().get(),
        workflow_job_id = admission.job_id().get(),
        delivery_id = delivery_id.encode_lower(&mut delivery_buffer),
        step_count = admission.step_count(),
        step_limit,
        "completed workflow-job trace rejected"
    );
}

async fn observe_webhook_request(
    State(metrics): State<Metrics>,
    request: Request,
    next: Next,
) -> Response {
    let repository_context = request
        .extensions()
        .get::<RequestRepositoryContext>()
        .cloned()
        .expect("HTTP middleware installs repository context");
    let started_at = Instant::now();
    let response = next.run(request).await;
    let result = result_for_status(response.status());
    metrics.observe_request(
        repository_context.repository(),
        result,
        started_at.elapsed(),
    );
    debug!(
        target: LOCAL_ONLY_LOG_TARGET,
        parent: None,
        result = result.as_str(),
        "GitHub webhook request processed"
    );
    response
}

fn record_queue_state_failure(state: &AppState, repository_name: &CanonicalRepositoryName) {
    state
        .metrics()
        .record_failure(Some(repository_name), FailureStage::QueueState);
    let error = AppError::webhook_unavailable();
    let error_correlation_id = error
        .correlation_id()
        .expect("webhook dependency failures carry a correlation ID");
    error!(
        parent: None,
        stage = FailureStage::QueueState.as_str(),
        result = WebhookResult::Unavailable.as_str(),
        %error_correlation_id,
        "GitHub webhook processing failed"
    );
}

fn unavailable_error(
    state: &AppState,
    repository_name: Option<&CanonicalRepositoryName>,
    stage: FailureStage,
) -> AppError {
    state.metrics().record_failure(repository_name, stage);
    let error = AppError::webhook_unavailable();
    let error_correlation_id = error
        .correlation_id()
        .expect("webhook dependency failures carry a correlation ID");
    error!(
        parent: None,
        stage = stage.as_str(),
        result = WebhookResult::Unavailable.as_str(),
        %error_correlation_id,
        "GitHub webhook processing failed"
    );
    error
}

fn validate_content_type(parts: &Parts) -> Result<(), AppError> {
    match parts.headers.get(CONTENT_TYPE) {
        None => Err(AppError::invalid_webhook()),
        Some(value) if value == JSON_CONTENT_TYPE => Ok(()),
        Some(_) => Err(AppError::unsupported_webhook_media_type()),
    }
}

fn required_header<'headers>(
    headers: &'headers HeaderMap,
    name: &'static str,
) -> Result<&'headers str, AppError> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(AppError::invalid_webhook)
}

fn map_body_rejection(rejection: BytesRejection) -> AppError {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        AppError::webhook_payload_too_large()
    } else {
        AppError::invalid_webhook()
    }
}

fn result_for_status(status: StatusCode) -> WebhookResult {
    let result = match status {
        StatusCode::NO_CONTENT => Some(WebhookResult::Accepted),
        StatusCode::BAD_REQUEST => Some(WebhookResult::Malformed),
        StatusCode::UNAUTHORIZED => Some(WebhookResult::Unauthorized),
        StatusCode::PAYLOAD_TOO_LARGE => Some(WebhookResult::TooLarge),
        StatusCode::UNSUPPORTED_MEDIA_TYPE => Some(WebhookResult::Unsupported),
        StatusCode::SERVICE_UNAVAILABLE => Some(WebhookResult::Unavailable),
        _ => None,
    };
    if result.is_none() {
        error!(
            parent: None,
            status = status.as_u16(),
            result = WebhookResult::Unavailable.as_str(),
            "unexpected GitHub webhook response status"
        );
    }
    debug_assert!(
        result.is_some(),
        "unexpected GitHub webhook response status: {status}"
    );
    result.unwrap_or(WebhookResult::Unavailable)
}

#[cfg(test)]
mod tests {
    use axum::http::StatusCode;

    use super::result_for_status;

    #[test]
    #[should_panic(expected = "unexpected GitHub webhook response status")]
    fn unexpected_response_status_is_visible_in_debug_builds() {
        let _result = result_for_status(StatusCode::INTERNAL_SERVER_ERROR);
    }
}
