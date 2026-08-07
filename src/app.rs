use std::{
    future::{Future, IntoFuture},
    sync::Arc,
    time::Duration,
};

use axum::{
    extract::{MatchedPath, Request},
    middleware::{self, Next},
    response::Response,
    Router,
};
use sqlx::SqlitePool;
use tokio::{net::TcpListener, sync::watch};
use tracing::Instrument;

use crate::{
    api, health,
    metrics::{self, Metrics},
    retention::{run_retention, RetentionConfig},
    security::AdminAuthenticator,
    storage::{DeliveryStore, MergeQueueStore, RepositoryStore},
    telemetry::{
        trace::{self, Operation},
        WorkflowTraceEmitter,
    },
};

/// Immutable dependencies shared by all HTTP request handlers.
#[derive(Clone)]
pub struct AppState {
    repository_store: Arc<RepositoryStore>,
    admin_authenticator: Arc<AdminAuthenticator>,
    database_pool: SqlitePool,
    delivery_store: DeliveryStore,
    merge_queue_store: MergeQueueStore,
    metrics: Metrics,
    workflow_trace_emitter: WorkflowTraceEmitter,
    webhook_body_limit_bytes: usize,
    workflow_job_max_steps: usize,
}

impl AppState {
    /// Creates application state from initialized repository and authentication services.
    pub fn new(
        repository_store: RepositoryStore,
        admin_authenticator: AdminAuthenticator,
        webhook_body_limit_bytes: usize,
        workflow_job_max_steps: usize,
    ) -> Self {
        let database_pool = repository_store.pool().clone();
        let delivery_store = DeliveryStore::new(database_pool.clone());
        let merge_queue_store = MergeQueueStore::new(database_pool.clone());
        Self {
            repository_store: Arc::new(repository_store),
            admin_authenticator: Arc::new(admin_authenticator),
            database_pool,
            delivery_store,
            merge_queue_store,
            metrics: Metrics::new(),
            workflow_trace_emitter: WorkflowTraceEmitter::disabled(),
            webhook_body_limit_bytes,
            workflow_job_max_steps,
        }
    }

    /// Returns application state updated with a shared metrics registry.
    pub fn with_metrics(mut self, metrics: Metrics) -> Self {
        self.metrics = metrics;
        self
    }

    /// Returns application state updated with the configured workflow trace emitter.
    pub fn with_workflow_trace_emitter(
        mut self,
        workflow_trace_emitter: WorkflowTraceEmitter,
    ) -> Self {
        self.workflow_trace_emitter = workflow_trace_emitter;
        self
    }

    /// Initializes the configured-repository gauge from durable storage.
    ///
    /// Call this after migrations and before binding the HTTP listener so readiness cannot be
    /// served with a stale startup value.
    ///
    /// # Errors
    ///
    /// Returns a redacted repository persistence error when SQLite cannot count records.
    pub async fn initialize_repository_metrics(
        &self,
    ) -> Result<(), crate::storage::RepositoryStoreError> {
        let count = self.repository_store.count().await?;
        self.metrics.set_repository_configurations(count);
        Ok(())
    }

    /// Returns encrypted repository persistence.
    pub fn repository_store(&self) -> &RepositoryStore {
        &self.repository_store
    }

    /// Returns the independent administrator credential verifier.
    pub fn admin_authenticator(&self) -> &AdminAuthenticator {
        &self.admin_authenticator
    }

    /// Returns durable authenticated-delivery claim persistence.
    pub fn delivery_store(&self) -> &DeliveryStore {
        &self.delivery_store
    }

    /// Returns durable pull-request merge-queue attempt persistence.
    pub fn merge_queue_store(&self) -> &MergeQueueStore {
        &self.merge_queue_store
    }

    /// Returns the shared bounded metrics component.
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// Returns the configured explicit-time historical workflow trace emitter.
    pub fn workflow_trace_emitter(&self) -> &WorkflowTraceEmitter {
        &self.workflow_trace_emitter
    }

    /// Returns the maximum reported steps accepted for one completed workflow-job trace.
    pub fn workflow_job_max_steps(&self) -> usize {
        self.workflow_job_max_steps
    }
}

/// Builds the composable application router.
pub fn build_router(state: AppState) -> Router {
    let webhook_body_limit_bytes = state.webhook_body_limit_bytes;
    let webhook_metrics = state.metrics.clone();
    health::router(state.database_pool.clone())
        .merge(
            api::router(webhook_body_limit_bytes, webhook_metrics)
                .merge(metrics::router())
                .with_state(state),
        )
        .layer(middleware::from_fn(observe_http_request))
}

async fn observe_http_request(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let route = request.extensions().get::<MatchedPath>();
    let span = trace::operation_span(Operation::HttpRequest);
    trace::set_http_method(&span, &method);
    trace::set_http_route(&span, route);

    let response = next.run(request).instrument(span.clone()).await;
    trace::set_http_response(&span, response.status());
    response
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
    retention_config: RetentionConfig,
) -> std::io::Result<ShutdownOutcome>
where
    S: Future<Output = ()>,
{
    let delivery_store = state.delivery_store.clone();
    let merge_queue_store = state.merge_queue_store.clone();
    serve_router_with_background_shutdown(
        listener,
        build_router(state),
        shutdown,
        shutdown_timeout,
        move |cancellation| {
            run_retention(
                delivery_store,
                merge_queue_store,
                retention_config,
                cancellation,
            )
        },
    )
    .await
}

#[cfg(test)]
async fn serve_router_with_shutdown<S>(
    listener: TcpListener,
    router: Router,
    shutdown: S,
    shutdown_timeout: Duration,
) -> std::io::Result<ShutdownOutcome>
where
    S: Future<Output = ()>,
{
    serve_router_with_background_shutdown(
        listener,
        router,
        shutdown,
        shutdown_timeout,
        |mut cancellation| async move {
            while !*cancellation.borrow() {
                if cancellation.changed().await.is_err() {
                    return;
                }
            }
        },
    )
    .await
}

async fn serve_router_with_background_shutdown<S, F, B>(
    listener: TcpListener,
    router: Router,
    shutdown: S,
    shutdown_timeout: Duration,
    background: F,
) -> std::io::Result<ShutdownOutcome>
where
    S: Future<Output = ()>,
    F: FnOnce(watch::Receiver<bool>) -> B,
    B: Future<Output = ()> + Send + 'static,
{
    let (cancellation_sender, cancellation_receiver) = watch::channel(false);
    let mut server_cancellation = cancellation_receiver.clone();
    let server = axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            while !*server_cancellation.borrow() {
                if server_cancellation.changed().await.is_err() {
                    return;
                }
            }
        })
        .into_future();
    let mut background = tokio::spawn(background(cancellation_receiver));
    tokio::pin!(server);
    tokio::pin!(shutdown);

    tokio::select! {
        result = &mut server => {
            cancellation_sender.send_replace(true);
            let background_result = background
                .await
                .map_err(|_| std::io::Error::other("background lifecycle task failed"));
            prioritize_server_result(result, background_result)
                .map(|()| ShutdownOutcome::Completed)
        }
        () = &mut shutdown => {
            cancellation_sender.send_replace(true);
            let drain = async {
                let server_result = (&mut server).await;
                let background_result = (&mut background)
                    .await
                    .map_err(|_| std::io::Error::other("background lifecycle task failed"));
                prioritize_server_result(server_result, background_result)
            };
            match tokio::time::timeout(shutdown_timeout, drain).await {
                Ok(result) => result.map(|()| ShutdownOutcome::Completed),
                Err(_) => {
                    background.abort();
                    Ok(ShutdownOutcome::TimedOut)
                }
            }
        }
    }
}

fn prioritize_server_result(
    server_result: std::io::Result<()>,
    background_result: std::io::Result<()>,
) -> std::io::Result<()> {
    server_result.and(background_result)
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use axum::{
        body::{to_bytes, Body},
        extract::State,
        http::{header::CONTENT_TYPE, Request, StatusCode},
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
        metrics::{Metrics, TelemetryDropReason, TelemetrySignal},
        security::{AdminAuthenticator, AdminToken, MasterKey, RepositorySecretCipher},
        storage::RepositoryStore,
    };

    use super::{
        build_router, prioritize_server_result, serve_router_with_background_shutdown,
        serve_router_with_shutdown, AppState, ShutdownOutcome,
    };

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
            2_097_152,
            256,
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
    async fn application_state_exposes_workflow_job_step_limit() {
        let state = app_state().await;

        assert_eq!(state.workflow_job_max_steps(), 256);
    }

    #[tokio::test]
    async fn installed_metrics_are_served() {
        let metrics = Metrics::new();
        metrics.record_telemetry_drops(TelemetrySignal::Trace, TelemetryDropReason::QueueFull, 1);
        let response = build_router(app_state().await.with_metrics(metrics))
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .expect("request is valid"),
            )
            .await
            .expect("router serves request");
        let body = to_bytes(response.into_body(), 128 * 1_024)
            .await
            .expect("metrics response body is readable");
        let exposition = String::from_utf8(body.to_vec()).expect("metrics response is UTF-8");

        assert!(exposition.contains(
            "github_telemetry_dropped_records_total{signal=\"trace\",reason=\"queue_full\"} 1"
        ));
    }

    #[tokio::test]
    async fn metrics_endpoint_is_public_and_exposes_every_required_instrument() {
        const OPEN_METRICS_CONTENT_TYPE: &str =
            "application/openmetrics-text; version=1.0.0; charset=utf-8";
        let response = build_router(app_state().await)
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .expect("request is valid"),
            )
            .await
            .expect("router serves request");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE),
            Some(&OPEN_METRICS_CONTENT_TYPE.parse().expect("header is valid"))
        );
        let body = to_bytes(response.into_body(), 128 * 1_024)
            .await
            .expect("metrics response body is readable");
        let exposition = String::from_utf8(body.to_vec()).expect("metrics response is UTF-8");
        for metric_name in [
            "github_webhook_requests_total",
            "github_webhook_events_total",
            "github_webhook_processing_duration_seconds",
            "github_webhook_request_body_bytes",
            "github_webhook_duplicates_total",
            "github_webhook_processing_failures_total",
            "github_repository_configurations",
            "github_merge_group_events_total",
            "github_merge_queue_pr_outcomes_total",
            "github_merge_queue_attempt_duration_seconds",
            "github_merge_queue_transition_failures_total",
            "github_workflow_job_steps",
            "github_workflow_job_trace_rejections_total",
            "github_telemetry_export_failures_total",
            "github_telemetry_dropped_records_total",
        ] {
            assert!(
                exposition.contains(metric_name),
                "missing {metric_name:?} in:\n{exposition}"
            );
        }
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

    #[test]
    fn server_io_error_takes_precedence_over_background_join_error() {
        let error = prioritize_server_result(
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "server failure",
            )),
            Err(std::io::Error::other("background failure")),
        )
        .expect_err("combined lifecycle must fail");

        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionAborted);
        assert_eq!(error.to_string(), "server failure");
    }

    #[tokio::test]
    async fn graceful_lifecycle_shares_drain_deadline_with_request_and_background_work() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("ephemeral listener binds");
        let address = listener.local_addr().expect("listener has an address");
        let request_state = DrainState {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
        };
        let router = Router::new()
            .route("/slow", get(slow_handler))
            .with_state(request_state.clone());
        let (shutdown_sender, shutdown_receiver) = oneshot::channel();
        let background_started = Arc::new(Notify::new());
        let background_release = Arc::new(Notify::new());
        let task_background_started = Arc::clone(&background_started);
        let task_background_release = Arc::clone(&background_release);
        let server = tokio::spawn(serve_router_with_background_shutdown(
            listener,
            router,
            async move {
                drop(shutdown_receiver.await);
            },
            Duration::from_secs(2),
            move |mut cancellation| async move {
                while !*cancellation.borrow() {
                    if cancellation.changed().await.is_err() {
                        return;
                    }
                }
                task_background_started.notify_one();
                task_background_release.notified().await;
            },
        ));
        let mut stream = TcpStream::connect(address)
            .await
            .expect("client connects to server");
        stream
            .write_all(b"GET /slow HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("request is written");
        request_state.started.notified().await;

        shutdown_sender.send(()).expect("server receives shutdown");
        background_started.notified().await;
        assert!(!server.is_finished());
        background_release.notify_one();
        tokio::task::yield_now().await;
        assert!(!server.is_finished());
        request_state.release.notify_one();
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
