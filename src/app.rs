use std::{
    future::{Future, IntoFuture},
    sync::Arc,
    time::Duration,
};

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

/// The normalized result of serving after a graceful-shutdown request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownOutcome {
    /// Axum stopped accepting connections and every in-flight request completed.
    Completed,
    /// The configured drain duration elapsed and remaining requests were dropped.
    TimedOut,
}

/// Serves until shutdown is requested, then bounds draining by `shutdown_timeout`.
///
/// The server continues accepting connections while the shutdown future is pending. Once it
/// resolves, Axum stops admission and receives at most `shutdown_timeout` to finish active
/// requests. Timing out drops the server future and closes remaining connections.
///
/// # Errors
///
/// Returns an I/O error when the HTTP server cannot accept or serve a connection.
pub async fn serve_with_shutdown<S>(
    listener: TcpListener,
    state: AppState,
    shutdown: S,
    shutdown_timeout: Duration,
) -> std::io::Result<ShutdownOutcome>
where
    S: Future<Output = ()>,
{
    serve_router_with_shutdown(listener, build_router(state), shutdown, shutdown_timeout).await
}

async fn serve_router_with_shutdown<S>(
    listener: TcpListener,
    router: Router,
    shutdown: S,
    shutdown_timeout: Duration,
) -> std::io::Result<ShutdownOutcome>
where
    S: Future<Output = ()>,
{
    let (graceful_sender, graceful_receiver) = tokio::sync::oneshot::channel();
    let server = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            drop(graceful_receiver.await);
        })
        .into_future();
    tokio::pin!(server);
    tokio::pin!(shutdown);

    tokio::select! {
        result = &mut server => result.map(|()| ShutdownOutcome::Completed),
        () = &mut shutdown => {
            let _send_result = graceful_sender.send(());
            match tokio::time::timeout(shutdown_timeout, &mut server).await {
                Ok(result) => result.map(|()| ShutdownOutcome::Completed),
                Err(_) => Ok(ShutdownOutcome::TimedOut),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use axum::{
        body::Body,
        extract::State,
        http::{Request, StatusCode},
        routing::get,
        Router,
    };
    use sqlx::SqlitePool;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::{oneshot, Notify},
        task::JoinHandle,
    };
    use tower::ServiceExt;

    use crate::{
        security::{AdminAuthenticator, AdminToken, MasterKey, RepositorySecretCipher},
        storage::RepositoryStore,
    };

    use super::{build_router, serve_router_with_shutdown, AppState, ShutdownOutcome};

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

    #[derive(Clone)]
    struct DrainState {
        started: Arc<Notify>,
        release: Arc<Notify>,
    }

    async fn slow_handler(State(state): State<DrainState>) -> StatusCode {
        state.started.notify_one();
        state.release.notified().await;
        StatusCode::OK
    }

    struct DrainTest {
        state: DrainState,
        shutdown_sender: oneshot::Sender<()>,
        server: JoinHandle<std::io::Result<ShutdownOutcome>>,
        stream: TcpStream,
    }

    async fn start_drain_test() -> DrainTest {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral listener binds");
        let address = listener.local_addr().expect("listener has an address");
        let state = DrainState {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        };
        let router = Router::new()
            .route("/slow", get(slow_handler))
            .with_state(state.clone());
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let server = tokio::spawn(serve_router_with_shutdown(
            listener,
            router,
            async move {
                drop(shutdown_receiver.await);
            },
            Duration::from_secs(2),
        ));
        let mut stream = TcpStream::connect(address)
            .await
            .expect("client connects to server");
        stream
            .write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("request is written");
        state.started.notified().await;

        DrainTest {
            state,
            shutdown_sender,
            server,
            stream,
        }
    }

    #[tokio::test]
    async fn graceful_lifecycle_drains_an_in_flight_request() {
        let DrainTest {
            state,
            shutdown_sender,
            server,
            mut stream,
        } = start_drain_test().await;

        shutdown_sender.send(()).expect("server receives shutdown");
        state.release.notify_one();
        let mut response = Vec::new();
        stream
            .read_to_end(&mut response)
            .await
            .expect("drained response is read");

        assert!(String::from_utf8(response)
            .expect("response is UTF-8")
            .starts_with("HTTP/1.1 200 OK\r\n"));
        assert_eq!(
            server
                .await
                .expect("server task joins")
                .expect("server runs"),
            ShutdownOutcome::Completed
        );
    }

    #[tokio::test(start_paused = true)]
    async fn graceful_lifecycle_forces_exit_at_the_drain_timeout() {
        let DrainTest {
            shutdown_sender,
            server,
            stream: _stream,
            state: _state,
        } = start_drain_test().await;

        shutdown_sender.send(()).expect("server receives shutdown");
        // Let the server observe shutdown and arm its timeout before advancing virtual time.
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(3)).await;

        assert_eq!(
            server
                .await
                .expect("server task joins")
                .expect("server runs"),
            ShutdownOutcome::TimedOut
        );
    }
}
