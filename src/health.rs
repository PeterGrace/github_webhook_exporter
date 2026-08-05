use axum::{extract::State, http::StatusCode, routing::get, Router};
use sqlx::SqlitePool;
use tracing::warn;

use crate::{error::ErrorCorrelationId, storage::probe_database};

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
    match probe_database(&pool).await {
        Ok(()) => StatusCode::OK,
        Err(error) => {
            let error_correlation_id = ErrorCorrelationId::new();
            warn!(
                outcome = "not_ready",
                error = %error,
                %error_correlation_id,
                "SQLite readiness probe failed"
            );
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    use axum::{
        body::{to_bytes, Body},
        http::{Request, StatusCode},
    };
    use sqlx::SqlitePool;
    use tower::ServiceExt;

    use super::router;

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
        let output = SharedWriter::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(output.clone())
            .finish();
        let _subscriber_guard = tracing::subscriber::set_default(subscriber);
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
        let logs = output.contents();
        assert!(logs.contains("SQLite readiness probe failed"));
        assert!(logs.contains("outcome=\"not_ready\""));
    }
}
