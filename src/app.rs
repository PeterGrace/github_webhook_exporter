use std::sync::Arc;

use axum::Router;
use sqlx::SqlitePool;
use tokio::net::TcpListener;

use crate::{api, health, security::AdminAuthenticator, storage::RepositoryStore};

/// Immutable dependencies shared by all HTTP request handlers.
#[derive(Clone)]
pub struct AppState {
    repository_store: Arc<RepositoryStore>,
    admin_authenticator: Arc<AdminAuthenticator>,
    database_pool: SqlitePool,
}

impl AppState {
    /// Creates application state from initialized repository and authentication services.
    pub fn new(repository_store: RepositoryStore, admin_authenticator: AdminAuthenticator) -> Self {
        let database_pool = repository_store.pool().clone();
        Self {
            repository_store: Arc::new(repository_store),
            admin_authenticator: Arc::new(admin_authenticator),
            database_pool,
        }
    }

    /// Returns encrypted repository persistence.
    pub fn repository_store(&self) -> &RepositoryStore {
        &self.repository_store
    }

    /// Returns the independent administrator credential verifier.
    pub fn admin_authenticator(&self) -> &AdminAuthenticator {
        &self.admin_authenticator
    }
}

/// Builds the composable application router.
pub fn build_router(state: AppState) -> Router {
    health::router(state.database_pool.clone()).merge(api::router().with_state(state))
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
    use std::time::Duration;

    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use sqlx::SqlitePool;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        time::timeout,
    };
    use tower::ServiceExt;

    use crate::{
        security::{AdminAuthenticator, AdminToken, MasterKey, RepositorySecretCipher},
        storage::RepositoryStore,
    };

    use super::{build_router, serve, AppState};

    async fn app_state() -> AppState {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite opens");
        let key = MasterKey::from_slice(&[7_u8; 32]).expect("test key is valid");
        let cipher = RepositorySecretCipher::new(&key).expect("test cipher initializes");
        let token = AdminToken::new("admin-token".to_owned()).expect("test token is valid");
        AppState::new(
            RepositoryStore::new(pool, cipher),
            AdminAuthenticator::new(&token),
        )
    }

    #[tokio::test]
    async fn application_router_exposes_only_registered_feature_routes() {
        let router = build_router(app_state().await);

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
        let server = tokio::spawn(serve(listener, app_state().await));
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
