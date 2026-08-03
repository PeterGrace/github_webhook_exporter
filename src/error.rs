use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Serialize;
use thiserror::Error;

/// An error that can be converted into a safe HTTP response.
#[derive(Debug, Error)]
pub enum AppError {
    /// An unexpected application failure whose details must not reach clients.
    #[error("internal application error")]
    Internal(#[source] anyhow::Error),
}

impl AppError {
    /// Wraps an unexpected failure for safe response conversion.
    pub fn internal(error: impl Into<anyhow::Error>) -> Self {
        Self::Internal(error.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let body = ErrorResponse {
            code: "internal_error",
            message: "internal server error",
        };
        (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
    }
}

#[derive(Serialize)]
struct ErrorResponse {
    code: &'static str,
    message: &'static str,
}

#[cfg(test)]
mod tests {
    use axum::{body::to_bytes, http::StatusCode, response::IntoResponse};

    use super::AppError;

    #[tokio::test]
    async fn internal_error_response_hides_source_details() {
        const SENSITIVE_DETAIL: &str = "sensitive-internal-detail";
        let response = AppError::internal(anyhow::anyhow!(SENSITIVE_DETAIL)).into_response();

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
