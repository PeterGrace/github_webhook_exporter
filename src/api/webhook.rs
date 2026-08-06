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
use serde::Deserialize;
use time::OffsetDateTime;
use tracing::{error, info, Instrument};

use crate::{
    api::{merge_group::EventProjection, pull_request::QueueProcessor},
    app::AppState,
    domain::delivery::DeliveryId,
    error::AppError,
    metrics::{normalize_action, normalize_event_type, FailureStage, Metrics, WebhookResult},
    security::{
        CanonicalRepositoryName, WebhookAuthenticationError, WebhookAuthenticator, WebhookSignature,
    },
    storage::DeliveryClaim,
    telemetry::trace::{self, Operation, OperationOutcome, QueueEntity},
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
    event_type: String,
    delivery_id: DeliveryId,
    signature: WebhookSignature,
}

impl FromRequest<AppState> for WebhookRequest {
    type Rejection = AppError;

    async fn from_request(request: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        let (parts, body) = request.into_parts();
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
            return Err(unavailable_error(&state, FailureStage::Authentication));
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
                state.metrics().record_duplicate();
                Ok(OperationOutcome::Duplicate)
            }
            Ok(DeliveryClaim::New) => {
                state
                    .metrics()
                    .observe_event(event_type, action, request.body.len());
                if let Some(transition) =
                    event_projection.merge_group_transition(event_type, action)
                {
                    state
                        .metrics()
                        .record_merge_group_event(transition.action, transition.reason);
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
                        record_queue_state_failure(&state);
                    }
                }
                Ok(OperationOutcome::Success)
            }
            Err(_) => Err(unavailable_error(&state, FailureStage::DeliveryClaim)),
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

async fn observe_webhook_request(
    State(metrics): State<Metrics>,
    request: Request,
    next: Next,
) -> Response {
    let started_at = Instant::now();
    let response = next.run(request).await;
    let result = result_for_status(response.status());
    metrics.observe_request(result, started_at.elapsed());
    info!(
        parent: None,
        result = result.as_str(),
        "GitHub webhook request processed"
    );
    response
}

fn record_queue_state_failure(state: &AppState) {
    state.metrics().record_failure(FailureStage::QueueState);
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

fn unavailable_error(state: &AppState, stage: FailureStage) -> AppError {
    state.metrics().record_failure(stage);
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
