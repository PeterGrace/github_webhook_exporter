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
use tracing::{error, info};

use crate::{
    app::AppState,
    domain::delivery::DeliveryId,
    error::AppError,
    metrics::{normalize_action, normalize_event_type, FailureStage, Metrics, WebhookResult},
    security::{
        CanonicalRepositoryName, WebhookAuthenticationError, WebhookAuthenticator, WebhookSignature,
    },
    storage::DeliveryClaim,
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

#[derive(Deserialize)]
struct WebhookActionProjection {
    action: Option<String>,
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
    match WebhookAuthenticator::new(state.repository_store())
        .authenticate(
            &request.repository_name,
            &request.signature,
            request.body.as_ref(),
        )
        .await
    {
        Ok(()) => {}
        Err(WebhookAuthenticationError::Unauthorized) => {
            return Err(AppError::unauthorized_webhook());
        }
        Err(WebhookAuthenticationError::Unavailable) => {
            return Err(unavailable_error(&state, FailureStage::Authentication));
        }
    }

    let action_projection: WebhookActionProjection =
        serde_json::from_slice(&request.body).map_err(|_| AppError::invalid_webhook())?;

    match state.delivery_store().claim(&request.delivery_id).await {
        Ok(DeliveryClaim::Duplicate) => state.metrics().record_duplicate(),
        Ok(DeliveryClaim::New) => state.metrics().observe_event(
            normalize_event_type(&request.event_type),
            normalize_action(action_projection.action.as_deref()),
            request.body.len(),
        ),
        Err(_) => {
            return Err(unavailable_error(&state, FailureStage::DeliveryClaim));
        }
    }

    Ok(StatusCode::NO_CONTENT.into_response())
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
    info!(result = result.as_str(), "GitHub webhook request processed");
    response
}

fn unavailable_error(state: &AppState, stage: FailureStage) -> AppError {
    state.metrics().record_failure(stage);
    let error = AppError::webhook_unavailable();
    let error_correlation_id = error
        .correlation_id()
        .expect("webhook dependency failures carry a correlation ID");
    error!(
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
