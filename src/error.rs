use std::fmt;

use axum::{
    http::{header, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use tracing::error;

use crate::{
    security::AuthenticationError,
    storage::{RepositoryStoreError, StorageError},
};

/// An error that can be converted into a safe HTTP response.
pub enum AppError {
    /// Request syntax or input validation failed.
    InvalidRequest,
    /// Administrator authentication failed.
    Authentication(AuthenticationError),
    /// Repository persistence failed.
    RepositoryStore(RepositoryStoreError),
    /// An unexpected application failure whose details must not reach clients.
    Internal(anyhow::Error),
}

impl AppError {
    /// Creates a stable invalid-request failure.
    pub fn invalid_request() -> Self {
        Self::InvalidRequest
    }

    /// Wraps an administrator-authentication failure.
    pub fn authentication(error: AuthenticationError) -> Self {
        Self::Authentication(error)
    }

    /// Wraps a repository persistence failure.
    pub fn repository_store(error: RepositoryStoreError) -> Self {
        Self::RepositoryStore(error)
    }

    /// Wraps an unexpected failure for safe response conversion.
    pub fn internal(error: impl Into<anyhow::Error>) -> Self {
        Self::Internal(error.into())
    }
}

impl fmt::Debug for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidRequest => formatter.write_str("AppError::InvalidRequest"),
            Self::Authentication(_) => formatter.write_str("AppError::Authentication"),
            Self::RepositoryStore(_) => formatter.write_str("AppError::RepositoryStore"),
            Self::Internal(_) => formatter.write_str("AppError::Internal"),
        }
    }
}

impl fmt::Display for AppError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("request failed")
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        match self {
            Self::InvalidRequest => error_response(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "request is invalid",
            ),
            Self::Authentication(_) => {
                let mut response = error_response(
                    StatusCode::UNAUTHORIZED,
                    "unauthorized",
                    "authentication required",
                );
                response
                    .headers_mut()
                    .insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
                response
            }
            Self::RepositoryStore(error) => match error {
                RepositoryStoreError::NotFound => {
                    error_response(StatusCode::NOT_FOUND, "not_found", "repository not found")
                }
                RepositoryStoreError::Conflict => error_response(
                    StatusCode::CONFLICT,
                    "conflict",
                    "repository name already exists",
                ),
                RepositoryStoreError::EmptyMutation => error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_request",
                    "repository update must not be empty",
                ),
                RepositoryStoreError::Cryptographic(_)
                | RepositoryStoreError::Unavailable
                | RepositoryStoreError::InternalData
                | RepositoryStoreError::Internal(_) => internal_response(),
            },
            Self::Internal(_) => internal_response(),
        }
    }
}

fn internal_response() -> Response {
    error!(outcome = "internal_error", "request failed");
    error_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal_error",
        "internal server error",
    )
}

fn error_response(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    (status, Json(ErrorResponse { code, message })).into_response()
}

#[derive(Serialize)]
struct ErrorResponse {
    code: &'static str,
    message: &'static str,
}

impl From<StorageError> for AppError {
    fn from(error: StorageError) -> Self {
        Self::internal(error)
    }
}

#[cfg(test)]
mod tests {
    use axum::{body::to_bytes, http::StatusCode, response::IntoResponse};

    use super::AppError;

    #[tokio::test]
    async fn internal_error_response_hides_source_details() {
        const SENSITIVE_DETAIL: &str = "sensitive-internal-detail";
        let error = AppError::internal(anyhow::anyhow!(SENSITIVE_DETAIL));
        assert!(!format!("{error:?}").contains(SENSITIVE_DETAIL));
        let response = error.into_response();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let body = to_bytes(response.into_body(), 1024)
            .await
            .expect("response body is readable");
        let body = String::from_utf8(body.to_vec()).expect("response body is UTF-8");
        assert_eq!(
            body,
            r#"{"code":"internal_error","message":"internal server error"}"#
        );
        assert!(!body.contains(SENSITIVE_DETAIL));
    }
}
