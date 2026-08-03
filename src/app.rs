use std::sync::Arc;

use axum::Router;
use tokio::net::TcpListener;

use crate::config::RuntimeConfig;

/// Immutable dependencies shared by all HTTP request handlers.
#[derive(Clone)]
pub struct AppState {
    config: Arc<RuntimeConfig>,
}

impl AppState {
    /// Creates application state from validated runtime configuration.
    pub fn new(config: RuntimeConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }

    /// Returns the validated process configuration.
    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }
}

/// Builds the composable application router.
///
/// Feature issues extend this router with health, configuration, webhook, and metrics routes.
pub fn build_router(state: AppState) -> Router {
    Router::new().with_state(state)
}

/// Serves the application router on an already-bound TCP listener.
///
/// Accepting a listener keeps socket ownership explicit and allows callers to report bind failures
/// with application-level context.
///
/// # Errors
///
/// Returns an I/O error when the HTTP server cannot accept or serve a connection.
pub async fn serve(listener: TcpListener, state: AppState) -> std::io::Result<()> {
    axum::serve(listener, build_router(state)).await
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, ffi::OsString, time::Duration};

    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        time::timeout,
    };
    use tower::ServiceExt;

    use crate::config::RuntimeConfig;

    use super::{build_router, serve, AppState};

    fn runtime_config() -> RuntimeConfig {
        let variables = HashMap::from([
            (
                "GHE_DATABASE_PATH".to_owned(),
                OsString::from("/tmp/exporter.db"),
            ),
            (
                "GHE_MASTER_KEY".to_owned(),
                OsString::from(STANDARD.encode([7_u8; 32])),
            ),
            ("GHE_ADMIN_TOKEN".to_owned(), OsString::from("admin-token")),
        ]);
        RuntimeConfig::from_lookup(|variable| variables.get(variable).cloned())
            .expect("test configuration is valid")
    }

    #[tokio::test]
    async fn empty_application_router_is_ready_for_feature_routes() {
        let router = build_router(AppState::new(runtime_config()));

        let response = router
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(Body::empty())
                    .expect("request is valid"),
            )
            .await
            .expect("router serves request");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn server_accepts_http_requests_on_the_supplied_listener() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral listener binds");
        let address = listener.local_addr().expect("listener has an address");
        let server = tokio::spawn(serve(listener, AppState::new(runtime_config())));
        let mut stream = TcpStream::connect(address)
            .await
            .expect("client connects to server");
        stream
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("request is written");
        let mut response = Vec::new();

        timeout(Duration::from_secs(2), stream.read_to_end(&mut response))
            .await
            .expect("server responds before timeout")
            .expect("response is read");
        server.abort();
        let _cancelled = server.await;

        let response = String::from_utf8(response).expect("response is UTF-8");
        assert!(response.starts_with("HTTP/1.1 404 Not Found\r\n"));
    }
}
