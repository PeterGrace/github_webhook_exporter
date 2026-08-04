use axum::{extract::State, http::StatusCode, routing::get, Router};
use sqlx::SqlitePool;

use crate::storage::probe_database;

/// Builds the unauthenticated health-check router.
///
/// Liveness has no dependency extractor and never accesses SQLite. Readiness executes a minimal
/// query against the supplied migrated pool and converts every probe failure into an empty `503`
/// response so internal database details cannot reach clients.
pub fn router(pool: SqlitePool) -> Router {
    Router::new()
        .route("/health/live", get(live))
        .route("/health/ready", get(ready))
        .with_state(pool)
}

async fn live() -> StatusCode {
    StatusCode::OK
}

async fn ready(State(pool): State<SqlitePool>) -> StatusCode {
    if probe_database(&pool).await.is_ok() {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use sqlx::SqlitePool;
    use tower::ServiceExt;

    use super::router;

    #[tokio::test]
    async fn liveness_does_not_depend_on_database_availability() {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite opens");
        let app = router(pool.clone());
        pool.close().await;

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/health/live")
                    .body(Body::empty())
                    .expect("request is valid"),
            )
            .await
            .expect("router serves request");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readiness_reflects_database_availability_without_error_details() {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite opens");
        let app = router(pool.clone());

        let ready = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .expect("request is valid"),
            )
            .await
            .expect("router serves request");
        assert_eq!(ready.status(), StatusCode::OK);

        pool.close().await;
        let unavailable = app
            .oneshot(
                Request::builder()
                    .uri("/health/ready")
                    .body(Body::empty())
                    .expect("request is valid"),
            )
            .await
            .expect("router serves request");
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert!(to_bytes(unavailable.into_body(), 1)
            .await
            .expect("response body is readable")
            .is_empty());
    }
}
