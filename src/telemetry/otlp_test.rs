use std::{
    collections::{BTreeSet, HashMap},
    ffi::OsString,
    io::{self, Write},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
};

use axum::{
    body::{to_bytes, Body, Bytes},
    extract::State,
    http::{header, header::CONTENT_TYPE, HeaderMap, Method, Request, StatusCode},
    response::IntoResponse,
    routing::post,
    Router,
};
use hmac::{Hmac, Mac};
use opentelemetry_proto::tonic::{
    collector::{logs::v1::ExportLogsServiceRequest, trace::v1::ExportTraceServiceRequest},
    common::v1::any_value::Value as AttributeValue,
    trace::v1::Span,
};
use prost::Message;
use serde_json::Value;
use sha2::Sha256;
use sqlx::SqlitePool;
use tempfile::TempDir;
use tower::ServiceExt;
use tracing::instrument::WithSubscriber;

use crate::{
    app::{build_router, AppState},
    domain::{
        delivery::DeliveryId,
        merge_queue::{PullRequestNumber, QueueCompletion, QueueTimestamp},
        repository::RepositoryMutation,
    },
    security::{
        AdminAuthenticator, AdminToken, CanonicalRepositoryName, MasterKey, RepositorySecret,
        RepositorySecretCipher,
    },
    storage::{
        open_database, DeliveryStore, MergeQueueStore, RepositoryStore, RepositoryStoreError,
    },
};
use tokio::{
    net::TcpListener,
    sync::{watch, Notify},
};
use tracing::{Dispatch, Instrument};

use crate::config::TelemetryConfig;
use time::{Duration, OffsetDateTime};

use super::{build_runtime, trace, TelemetryState};

const QUEUE_CAPACITY: usize = 4;
const SATURATION_RECORDS: usize = 10;
const ADMIN_TOKEN: &str = "independent-admin-token";
const MASTER_KEY_BYTES: &[u8; 32] = b"MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM";
const WEBHOOK_SECRET: &str = "webhook-trace-secret-must-not-appear";
const WEBHOOK_REPOSITORY: &str = "owner/webhook-private-repository";
const WEBHOOK_SHA_40: &str = "0123456789ABCDEF0123456789abcdef01234567";
const WEBHOOK_SHA_64: &str = "ABCDEF0123456789abcdef0123456789ABCDEF0123456789abcdef0123456789";
const SECRET: &str = "forbidden-webhook-secret";
const SIGNATURE: &str = "sha256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const AUTHORIZATION: &str = "Bearer forbidden-admin-token";
const ACTOR: &str = "forbidden-actor";
const COMMAND: &str = "forbidden-command";
const RAW_REASON: &str = "forbidden-raw-reason";
const RAW_URL: &str = "https://forbidden.invalid/private";
const RAW_UNMATCHED_PATH: &str = "/forbidden-unmatched-private-path";
const PRIVACY_REPOSITORY: &str = "privacy-owner/privacy-repository";
const PRIVACY_SHA: &str = "abcdef0123456789abcdef0123456789abcdef01";
const PRIVACY_PR_NUMBER: i64 = 976_543_211;
const PRIVACY_REPOSITORY_SEQUENCE: i64 = 987_000_000;
const PRIVACY_MERGE_GROUP_DELIVERY: &str = "71000000-0000-4000-8000-000000000001";
const PRIVACY_ENQUEUE_DELIVERY: &str = "71000000-0000-4000-8000-000000000002";
const PRIVACY_DEQUEUE_DELIVERY: &str = "71000000-0000-4000-8000-000000000003";
const PRIVACY_AUTH_FAILURE_DELIVERY: &str = "71000000-0000-4000-8000-000000000004";
const PRIVACY_PROCESS_FAILURE_DELIVERY: &str = "71000000-0000-4000-8000-000000000005";
const PRIVACY_RETENTION_DELIVERY: &str = "71000000-0000-4000-8000-000000000006";

const RESOURCE_ATTRIBUTE_ALLOWLIST: &[&str] = &[
    "service.name",
    "service.version",
    "k8s.pod.name",
    "k8s.namespace.name",
];
const SPAN_ATTRIBUTE_ALLOWLIST: &[&str] = &[
    "http.request.method",
    "http.route",
    "http.response.status_code",
    "ghe.http.result",
    "ghe.operation.outcome",
    "ghe.config.operation",
    "ghe.webhook.event_type",
    "ghe.webhook.action",
    "ghe.queue.entity",
    "ghe.merge_group.action",
    "ghe.merge_group.reason",
    "ghe.queue.outcome",
    "ghe.queue.reason",
    "github.repository.name",
    "github.repository.id",
    "github.delivery.id",
    "github.pull_request.number",
    "github.commit.sha",
    "db.system.name",
    "db.operation.name",
];
const SPAN_EVENT_ALLOWLIST: &[(&str, &[&str])] = &[("operation.failure", &["ghe.failure.reason"])];

#[derive(Default)]
struct Captures {
    traces: Vec<ExportTraceServiceRequest>,
    logs: Vec<ExportLogsServiceRequest>,
}

struct ReceiverState {
    captures: Mutex<Captures>,
    started_requests: AtomicUsize,
    request_started: Notify,
    release_exports: watch::Sender<bool>,
}

type SharedReceiverState = Arc<ReceiverState>;

type OtlpTestGuard = tokio::sync::OwnedMutexGuard<()>;

fn otlp_test_lock() -> Arc<tokio::sync::Mutex<()>> {
    static LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();
    Arc::clone(LOCK.get_or_init(|| Arc::new(tokio::sync::Mutex::new(()))))
}

async fn receive_traces(
    State(state): State<SharedReceiverState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    validate_headers(&headers);
    let request = ExportTraceServiceRequest::decode(body).expect("trace request is protobuf");
    state
        .captures
        .lock()
        .expect("capture lock is available")
        .traces
        .push(request);
    wait_until_released(&state).await;
    protobuf_response()
}

async fn receive_logs(
    State(state): State<SharedReceiverState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    validate_headers(&headers);
    let request = ExportLogsServiceRequest::decode(body).expect("log request is protobuf");
    state
        .captures
        .lock()
        .expect("capture lock is available")
        .logs
        .push(request);
    wait_until_released(&state).await;
    protobuf_response()
}

fn validate_headers(headers: &HeaderMap) {
    assert_eq!(
        headers
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("application/x-protobuf")
    );
    assert_eq!(
        headers
            .get("x-test-token")
            .and_then(|value| value.to_str().ok()),
        Some("private-value")
    );
}

async fn wait_until_released(state: &ReceiverState) {
    let mut release = state.release_exports.subscribe();
    state.started_requests.fetch_add(1, Ordering::AcqRel);
    state.request_started.notify_one();
    while !*release.borrow() {
        release
            .changed()
            .await
            .expect("release sender remains available");
    }
}

fn protobuf_response() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(CONTENT_TYPE, "application/x-protobuf")],
        Bytes::new(),
    )
}

fn telemetry_config(endpoint: &str) -> TelemetryConfig {
    telemetry_config_with_queue_capacity(endpoint, QUEUE_CAPACITY)
}

fn telemetry_config_with_queue_capacity(endpoint: &str, queue_capacity: usize) -> TelemetryConfig {
    let queue_capacity = queue_capacity.to_string();
    let values = HashMap::from([
        ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint),
        ("OTEL_EXPORTER_OTLP_HEADERS", "x-test-token=private-value"),
        ("OTEL_EXPORTER_OTLP_TIMEOUT", "2000"),
        ("OTEL_SERVICE_NAME", "github-webhook-exporter-test"),
        (
            "OTEL_RESOURCE_ATTRIBUTES",
            "k8s.pod.name=exporter-0,k8s.namespace.name=tests,forbidden=value",
        ),
        ("GHE_OTEL_QUEUE_CAPACITY", queue_capacity.as_str()),
        ("GHE_OTEL_BATCH_SIZE", "1"),
    ]);
    TelemetryConfig::from_lookup(&mut |variable| values.get(variable).map(OsString::from))
        .expect("telemetry configuration is valid")
}

fn emit_records(dispatch: &Dispatch, range: std::ops::Range<usize>) {
    tracing::dispatcher::with_default(dispatch, || {
        for sequence in range {
            let span = tracing::info_span!(
                target: "github_webhook_exporter",
                "synthetic-span",
                sequence
            );
            let _guard = span.enter();
            tracing::info!(
                target: "github_webhook_exporter",
                sequence,
                "synthetic-log"
            );
        }
    });
}

fn trace_batch_sizes(requests: &[ExportTraceServiceRequest]) -> Vec<usize> {
    requests
        .iter()
        .map(|request| {
            request
                .resource_spans
                .iter()
                .flat_map(|resource| &resource.scope_spans)
                .map(|scope| scope.spans.len())
                .sum()
        })
        .collect()
}

fn log_batch_sizes(requests: &[ExportLogsServiceRequest]) -> Vec<usize> {
    requests
        .iter()
        .map(|request| {
            request
                .resource_logs
                .iter()
                .flat_map(|resource| &resource.scope_logs)
                .map(|scope| scope.log_records.len())
                .sum()
        })
        .collect()
}

fn resource_keys(request: &ExportTraceServiceRequest) -> Vec<&str> {
    request
        .resource_spans
        .iter()
        .filter_map(|spans| spans.resource.as_ref())
        .flat_map(|resource| resource.attributes.iter())
        .map(|attribute| attribute.key.as_str())
        .collect()
}

async fn wait_for_blocked_signals(state: &ReceiverState) {
    while state.started_requests.load(Ordering::Acquire) < 2 {
        state.request_started.notified().await;
    }
}

struct RunningReceiver {
    state: SharedReceiverState,
    address: std::net::SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

impl RunningReceiver {
    async fn start_released() -> Self {
        let (release_exports, _) = watch::channel(true);
        let state = Arc::new(ReceiverState {
            captures: Mutex::new(Captures::default()),
            started_requests: AtomicUsize::new(0),
            request_started: Notify::new(),
            release_exports,
        });
        let app = Router::new()
            .route("/v1/traces", post(receive_traces))
            .route("/v1/logs", post(receive_logs))
            .with_state(Arc::clone(&state));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test receiver binds");
        let address = listener
            .local_addr()
            .expect("receiver address is available");
        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("test receiver serves requests");
        });
        Self {
            state,
            address,
            task,
        }
    }

    fn endpoint(&self) -> String {
        format!("http://{}", self.address)
    }

    fn captured_requests(
        &self,
    ) -> (
        Vec<ExportTraceServiceRequest>,
        Vec<ExportLogsServiceRequest>,
    ) {
        let captures = self
            .state
            .captures
            .lock()
            .expect("capture lock is available");
        (captures.traces.clone(), captures.logs.clone())
    }

    fn clear_captured_requests(&self) {
        *self
            .state
            .captures
            .lock()
            .expect("capture lock is available") = Captures::default();
    }
}

impl Drop for RunningReceiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct RepositoryTraceFixture {
    _otlp_guard: OtlpTestGuard,
    receiver: RunningReceiver,
    runtime: super::TelemetryRuntime,
    dispatch: Dispatch,
    router: Router,
    pool: SqlitePool,
    _directory: TempDir,
}

impl RepositoryTraceFixture {
    async fn new() -> Self {
        let otlp_guard = otlp_test_lock().lock_owned().await;
        let receiver = RunningReceiver::start_released().await;
        let config = telemetry_config_with_queue_capacity(&receiver.endpoint(), 128);
        let (runtime, subscriber) =
            build_runtime("github_webhook_exporter=info", &config, io::sink)
                .expect("telemetry runtime initializes");
        let dispatch = Dispatch::new(subscriber);
        let directory = tempfile::tempdir().expect("temporary directory is created");
        let pool = open_database(&directory.path().join("repository-trace.db"))
            .await
            .expect("test database opens and migrates");
        let router = router_for_pool(pool.clone());
        Self {
            _otlp_guard: otlp_guard,
            receiver,
            runtime,
            dispatch,
            router,
            pool,
            _directory: directory,
        }
    }

    async fn request(
        &self,
        method: Method,
        uri: &str,
        authorization: Option<&str>,
        content_type: Option<&str>,
        body: Body,
    ) -> axum::response::Response {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        if let Some(value) = content_type {
            request = request.header(header::CONTENT_TYPE, value);
        }
        self.router
            .clone()
            .oneshot(request.body(body).expect("request is valid"))
            .with_subscriber(self.dispatch.clone())
            .await
            .expect("router serves request")
    }

    async fn authorized_json(
        &self,
        method: Method,
        uri: &str,
        body: Value,
    ) -> axum::response::Response {
        let serialized = serde_json::to_vec(&body).expect("request body serializes");
        self.request(
            method,
            uri,
            Some("Bearer independent-admin-token"),
            Some("application/json"),
            Body::from(serialized),
        )
        .await
    }

    fn force_flush(&self) -> CapturedSpans {
        tokio::task::block_in_place(|| self.runtime.force_flush().expect("providers flush"));
        let (traces, logs) = self.receiver.captured_requests();
        CapturedSpans::from_requests(traces, logs)
    }
}

#[derive(Clone, Default)]
struct CapturedOutput(Arc<Mutex<Vec<u8>>>);

impl CapturedOutput {
    fn text(&self) -> String {
        let bytes = self.0.lock().expect("output lock is available").clone();
        String::from_utf8(bytes).expect("captured output is UTF-8")
    }
}

impl Write for CapturedOutput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .map_err(|_| io::Error::other("output lock is poisoned"))?
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for CapturedOutput {
    type Writer = Self;

    fn make_writer(&'writer self) -> Self::Writer {
        self.clone()
    }
}

struct WebhookTraceFixture {
    _otlp_guard: OtlpTestGuard,
    receiver: RunningReceiver,
    runtime: super::TelemetryRuntime,
    dispatch: Dispatch,
    output: CapturedOutput,
    router: Router,
    pool: SqlitePool,
    _directory: TempDir,
}

impl WebhookTraceFixture {
    async fn new() -> Self {
        let otlp_guard = otlp_test_lock().lock_owned().await;
        let receiver = RunningReceiver::start_released().await;
        let config = telemetry_config_with_queue_capacity(&receiver.endpoint(), 128);
        let output = CapturedOutput::default();
        let (runtime, subscriber) =
            build_runtime("github_webhook_exporter=info", &config, output.clone())
                .expect("telemetry runtime initializes");
        let dispatch = Dispatch::new(subscriber);
        let directory = tempfile::tempdir().expect("temporary directory is created");
        let pool = open_database(&directory.path().join("webhook-trace.db"))
            .await
            .expect("test database opens and migrates");
        let master_key = MasterKey::from_slice(MASTER_KEY_BYTES).expect("test key is valid");
        let cipher = RepositorySecretCipher::new(&master_key).expect("test cipher initializes");
        let store = RepositoryStore::new(pool.clone(), cipher);
        store
            .create(
                CanonicalRepositoryName::new(WEBHOOK_REPOSITORY)
                    .expect("test repository name is valid"),
                RepositorySecret::new(WEBHOOK_SECRET.to_owned())
                    .expect("test webhook secret is valid"),
                true,
            )
            .with_subscriber(dispatch.clone())
            .await
            .expect("test repository is created");
        tokio::task::block_in_place(|| runtime.force_flush().expect("setup telemetry flushes"));
        receiver.clear_captured_requests();
        let admin_token = AdminToken::new(ADMIN_TOKEN.to_owned()).expect("test token is valid");
        let router = build_router(AppState::new(
            store,
            AdminAuthenticator::new(&admin_token),
            2_097_152,
        ));
        Self {
            _otlp_guard: otlp_guard,
            receiver,
            runtime,
            dispatch,
            output,
            router,
            pool,
            _directory: directory,
        }
    }

    async fn request(
        &self,
        method: Method,
        uri: &str,
        authorization: Option<&str>,
        content_type: Option<&str>,
        body: Body,
    ) -> axum::response::Response {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        if let Some(value) = content_type {
            request = request.header(header::CONTENT_TYPE, value);
        }
        self.router
            .clone()
            .oneshot(request.body(body).expect("request is valid"))
            .with_subscriber(self.dispatch.clone())
            .await
            .expect("router serves request")
    }

    async fn authorized_json(
        &self,
        method: Method,
        uri: &str,
        body: Value,
    ) -> axum::response::Response {
        let serialized = serde_json::to_vec(&body).expect("request body serializes");
        self.request(
            method,
            uri,
            Some("Bearer independent-admin-token"),
            Some("application/json"),
            Body::from(serialized),
        )
        .await
    }

    async fn webhook(
        &self,
        body: &[u8],
        event_type: &str,
        delivery_id: &str,
        secret: &str,
    ) -> axum::response::Response {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC key is valid");
        mac.update(body);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let request = Request::builder()
            .method(Method::POST)
            .uri("/webhooks/github")
            .header(CONTENT_TYPE, "application/json")
            .header("X-GitHub-Event", event_type)
            .header("X-GitHub-Delivery", delivery_id)
            .header("X-Hub-Signature-256", signature)
            .body(Body::from(body.to_vec()))
            .expect("webhook request is valid");
        self.router
            .clone()
            .oneshot(request)
            .with_subscriber(self.dispatch.clone())
            .await
            .expect("router serves webhook request")
    }

    async fn webhook_with_signature(
        &self,
        body: &[u8],
        event_type: &str,
        delivery_id: &str,
        signature: &str,
    ) -> axum::response::Response {
        let request = Request::builder()
            .method(Method::POST)
            .uri("/webhooks/github")
            .header(CONTENT_TYPE, "application/json")
            .header("X-GitHub-Event", event_type)
            .header("X-GitHub-Delivery", delivery_id)
            .header("X-Hub-Signature-256", signature)
            .body(Body::from(body.to_vec()))
            .expect("webhook request is valid");
        self.router
            .clone()
            .oneshot(request)
            .with_subscriber(self.dispatch.clone())
            .await
            .expect("router serves webhook request")
    }

    async fn metrics_text(&self) -> String {
        let response = self
            .router
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .expect("metrics request is valid"),
            )
            .with_subscriber(self.dispatch.clone())
            .await
            .expect("router serves metrics request");
        String::from_utf8(
            to_bytes(response.into_body(), 1_000_000)
                .await
                .expect("metrics response is readable")
                .to_vec(),
        )
        .expect("metrics response is UTF-8")
    }

    fn flush(&self) {
        tokio::task::block_in_place(|| self.runtime.force_flush().expect("providers flush"));
    }

    fn force_flush(&self) -> CapturedSpans {
        let dropped_traces = self.runtime.dropped_trace_records();
        self.flush();
        assert_eq!(dropped_traces, 0, "test trace queue must not drop records");
        let (traces, logs) = self.receiver.captured_requests();
        CapturedSpans::from_requests(traces, logs)
    }
}

fn router_for_pool(pool: SqlitePool) -> Router {
    let master_key = MasterKey::from_slice(MASTER_KEY_BYTES).expect("test key is valid");
    let cipher = RepositorySecretCipher::new(&master_key).expect("test cipher initializes");
    let admin_token = AdminToken::new(ADMIN_TOKEN.to_owned()).expect("test token is valid");
    build_router(AppState::new(
        RepositoryStore::new(pool, cipher),
        AdminAuthenticator::new(&admin_token),
        2_097_152,
    ))
}

#[derive(Debug)]
struct CapturedSpans {
    spans: Vec<Span>,
    resource_attribute_keys: BTreeSet<String>,
    serialized: Vec<u8>,
    serialized_logs: Vec<u8>,
}

impl CapturedSpans {
    fn from_requests(
        requests: Vec<ExportTraceServiceRequest>,
        log_requests: Vec<ExportLogsServiceRequest>,
    ) -> Self {
        let spans = requests
            .iter()
            .flat_map(|request| &request.resource_spans)
            .flat_map(|resource| &resource.scope_spans)
            .flat_map(|scope| scope.spans.iter().cloned())
            .collect();
        let resource_attribute_keys = requests
            .iter()
            .flat_map(|request| &request.resource_spans)
            .filter_map(|resource_spans| resource_spans.resource.as_ref())
            .flat_map(|resource| &resource.attributes)
            .chain(
                log_requests
                    .iter()
                    .flat_map(|request| &request.resource_logs)
                    .filter_map(|resource_logs| resource_logs.resource.as_ref())
                    .flat_map(|resource| &resource.attributes),
            )
            .map(|attribute| attribute.key.clone())
            .collect();
        let serialized = requests
            .into_iter()
            .flat_map(|request| request.encode_to_vec())
            .collect();
        let serialized_logs = log_requests
            .into_iter()
            .flat_map(|request| request.encode_to_vec())
            .collect();
        Self {
            spans,
            resource_attribute_keys,
            serialized,
            serialized_logs,
        }
    }

    fn http_request(&self, method: &str, route: &str, status_code: i64) -> &Span {
        self.spans
            .iter()
            .find(|span| {
                span.name == "http.request"
                    && string_attribute(span, "http.request.method") == Some(method)
                    && string_attribute(span, "http.route") == Some(route)
                    && i64_attribute(span, "http.response.status_code") == Some(status_code)
            })
            .unwrap_or_else(|| {
                let summary: Vec<_> = self
                    .spans
                    .iter()
                    .map(|span| {
                        (
                            span.name.as_str(),
                            string_attribute(span, "http.request.method"),
                            string_attribute(span, "http.route"),
                            i64_attribute(span, "http.response.status_code"),
                        )
                    })
                    .collect();
                panic!("matching HTTP request span is exported: {summary:?}")
            })
    }

    fn webhook_request_for_delivery(&self, delivery_id: &str) -> &Span {
        self.spans
            .iter()
            .find(|span| {
                span.name == "http.request"
                    && self.spans.iter().any(|candidate| {
                        candidate.name == "github.webhook.authenticate"
                            && candidate.parent_span_id == span.span_id
                            && string_attribute(candidate, "github.delivery.id")
                                == Some(delivery_id)
                    })
            })
            .unwrap_or_else(|| {
                let summary: Vec<_> = self
                    .spans
                    .iter()
                    .map(|span| {
                        (
                            span.name.as_str(),
                            string_attribute(span, "github.delivery.id"),
                            span.span_id.as_slice(),
                            span.parent_span_id.as_slice(),
                        )
                    })
                    .collect();
                panic!("webhook request span for delivery is exported: {summary:?}")
            })
    }

    fn one_named(&self, name: &str) -> &Span {
        let matches = self
            .spans
            .iter()
            .filter(|span| span.name == name)
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1, "span count for {name}");
        matches[0]
    }

    fn child_named(&self, parent: &Span, name: &str) -> &Span {
        self.spans
            .iter()
            .find(|span| span.name == name && span.parent_span_id == parent.span_id)
            .expect("matching child span is exported")
    }

    fn children<'spans>(&'spans self, parent: &'spans Span) -> impl Iterator<Item = &'spans Span> {
        self.spans
            .iter()
            .filter(move |span| span.parent_span_id == parent.span_id)
    }

    fn child_count(&self, parent: &Span, name: &str) -> usize {
        self.spans
            .iter()
            .filter(|span| span.name == name && span.parent_span_id == parent.span_id)
            .count()
    }

    fn descendant_named(&self, parent: &Span, name: &str, entity: &str) -> &Span {
        self.spans
            .iter()
            .find(|span| {
                span.name == name
                    && (string_attribute(span, "ghe.queue.entity") == Some(entity)
                        || string_attribute(span, "db.operation.name") == Some(entity))
                    && self.is_descendant(span, parent)
            })
            .unwrap_or_else(|| {
                let summary = self
                    .spans
                    .iter()
                    .map(|span| {
                        (
                            span.name.as_str(),
                            string_attribute(span, "ghe.queue.entity"),
                            string_attribute(span, "db.operation.name"),
                        )
                    })
                    .collect::<Vec<_>>();
                panic!("matching descendant span {name:?} for {entity:?} is exported: {summary:?}")
            })
    }

    fn descendant_count(&self, parent: &Span, name: &str) -> usize {
        self.spans
            .iter()
            .filter(|span| span.name == name && self.is_descendant(span, parent))
            .count()
    }

    fn is_descendant(&self, span: &Span, ancestor: &Span) -> bool {
        let mut parent_span_id = span.parent_span_id.as_slice();
        while !parent_span_id.is_empty() {
            if parent_span_id == ancestor.span_id {
                return true;
            }
            let Some(parent) = self
                .spans
                .iter()
                .find(|candidate| candidate.span_id == parent_span_id)
            else {
                return false;
            };
            parent_span_id = parent.parent_span_id.as_slice();
        }
        false
    }

    fn assert_absent(&self, value: &str) {
        let serialized = String::from_utf8_lossy(&self.serialized);
        assert!(
            !serialized.contains(value),
            "serialized OTLP trace requests must not contain {value:?}"
        );
    }

    fn assert_logs_absent(&self, value: &str) {
        let serialized = String::from_utf8_lossy(&self.serialized_logs);
        assert!(
            !serialized.contains(value),
            "serialized OTLP log requests must not contain {value:?}"
        );
    }

    fn assert_approved_attribute_keys(&self) {
        for key in &self.resource_attribute_keys {
            assert!(
                RESOURCE_ATTRIBUTE_ALLOWLIST.contains(&key.as_str()),
                "unapproved OTLP resource attribute key: {key}"
            );
        }
        for span in &self.spans {
            for attribute in &span.attributes {
                assert!(
                    SPAN_ATTRIBUTE_ALLOWLIST.contains(&attribute.key.as_str()),
                    "unapproved attribute key {:?} on span {:?}",
                    attribute.key,
                    span.name
                );
            }
            for event in &span.events {
                let (_, allowed_keys) = SPAN_EVENT_ALLOWLIST
                    .iter()
                    .find(|(name, _)| *name == event.name)
                    .unwrap_or_else(|| panic!("unapproved span event name: {:?}", event.name));
                for attribute in &event.attributes {
                    assert!(
                        allowed_keys.contains(&attribute.key.as_str()),
                        "unapproved attribute key {:?} on span event {:?}",
                        attribute.key,
                        event.name
                    );
                }
            }
        }
    }
}

fn string_attribute<'span>(span: &'span Span, key: &str) -> Option<&'span str> {
    span.attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .and_then(|attribute| attribute.value.as_ref())
        .and_then(|value| match value.value.as_ref() {
            Some(AttributeValue::StringValue(value)) => Some(value.as_str()),
            Some(AttributeValue::IntValue(_))
            | Some(AttributeValue::DoubleValue(_))
            | Some(AttributeValue::BoolValue(_))
            | Some(AttributeValue::ArrayValue(_))
            | Some(AttributeValue::KvlistValue(_))
            | Some(AttributeValue::BytesValue(_))
            | Some(AttributeValue::StringValueStrindex(_))
            | None => None,
        })
}

fn event_string_attribute<'event>(
    event: &'event opentelemetry_proto::tonic::trace::v1::span::Event,
    key: &str,
) -> Option<&'event str> {
    event
        .attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .and_then(|attribute| attribute.value.as_ref())
        .and_then(|value| match value.value.as_ref() {
            Some(AttributeValue::StringValue(value)) => Some(value.as_str()),
            Some(AttributeValue::IntValue(_))
            | Some(AttributeValue::DoubleValue(_))
            | Some(AttributeValue::BoolValue(_))
            | Some(AttributeValue::ArrayValue(_))
            | Some(AttributeValue::KvlistValue(_))
            | Some(AttributeValue::BytesValue(_))
            | Some(AttributeValue::StringValueStrindex(_))
            | None => None,
        })
}

fn i64_attribute(span: &Span, key: &str) -> Option<i64> {
    span.attributes
        .iter()
        .find(|attribute| attribute.key == key)
        .and_then(|attribute| attribute.value.as_ref())
        .and_then(|value| match value.value.as_ref() {
            Some(AttributeValue::IntValue(value)) => Some(*value),
            Some(AttributeValue::StringValue(_))
            | Some(AttributeValue::DoubleValue(_))
            | Some(AttributeValue::BoolValue(_))
            | Some(AttributeValue::ArrayValue(_))
            | Some(AttributeValue::KvlistValue(_))
            | Some(AttributeValue::BytesValue(_))
            | Some(AttributeValue::StringValueStrindex(_))
            | None => None,
        })
}

async fn response_json(response: axum::response::Response) -> Value {
    serde_json::from_slice(
        &to_bytes(response.into_body(), 70_000)
            .await
            .expect("response body is readable"),
    )
    .expect("response body is JSON")
}

fn assert_attribute(span: &Span, key: &str, value: &str) {
    assert_eq!(string_attribute(span, key), Some(value));
}

fn assert_i64_attribute(span: &Span, key: &str, value: i64) {
    assert_eq!(i64_attribute(span, key), Some(value));
}

mod sqlite {
    use super::*;

    const SQLITE_TRACE_QUEUE_CAPACITY: usize = 128;
    const ENQUEUED_AT: &str = "2026-08-05T10:00:00.125Z";
    const COMPLETED_AT: &str = "2026-08-05T10:05:00.875Z";

    struct SqliteTraceFixture {
        _otlp_guard: OtlpTestGuard,
        receiver: RunningReceiver,
        runtime: super::super::TelemetryRuntime,
        dispatch: Dispatch,
        pool: SqlitePool,
        database_path: String,
        repository_store: RepositoryStore,
        delivery_store: DeliveryStore,
        merge_queue_store: MergeQueueStore,
        _directory: TempDir,
    }

    impl SqliteTraceFixture {
        async fn new() -> Self {
            let otlp_guard = otlp_test_lock().lock_owned().await;
            let receiver = RunningReceiver::start_released().await;
            let config = telemetry_config_with_queue_capacity(
                &receiver.endpoint(),
                SQLITE_TRACE_QUEUE_CAPACITY,
            );
            let (runtime, subscriber) =
                build_runtime("github_webhook_exporter=info", &config, io::sink)
                    .expect("telemetry runtime initializes");
            let dispatch = Dispatch::new(subscriber);
            let directory = tempfile::tempdir().expect("temporary directory is created");
            let database_path = directory.path().join("sqlite-trace.db");
            let pool = open_database(&database_path)
                .await
                .expect("test database opens and migrates");
            let master_key = MasterKey::from_slice(MASTER_KEY_BYTES).expect("test key is valid");
            let cipher = RepositorySecretCipher::new(&master_key).expect("test cipher initializes");
            let repository_store = RepositoryStore::new(pool.clone(), cipher);
            let delivery_store = DeliveryStore::new(pool.clone());
            let merge_queue_store = MergeQueueStore::new(pool.clone());

            Self {
                _otlp_guard: otlp_guard,
                receiver,
                runtime,
                dispatch,
                pool,
                database_path: database_path.display().to_string(),
                repository_store,
                delivery_store,
                merge_queue_store,
                _directory: directory,
            }
        }

        async fn traced<T>(&self, future: impl std::future::Future<Output = T>) -> T {
            future.with_subscriber(self.dispatch.clone()).await
        }

        fn force_flush(&self) -> CapturedSpans {
            tokio::task::block_in_place(|| self.runtime.force_flush().expect("providers flush"));
            let (traces, logs) = self.receiver.captured_requests();
            CapturedSpans::from_requests(traces, logs)
        }
    }

    fn name(value: &str) -> CanonicalRepositoryName {
        CanonicalRepositoryName::new(value).expect("test repository name is valid")
    }

    fn secret(value: &str) -> RepositorySecret {
        RepositorySecret::new(value.to_owned()).expect("test repository secret is valid")
    }

    fn queue_timestamp(value: &str) -> QueueTimestamp {
        QueueTimestamp::parse(value).expect("test queue timestamp is valid")
    }

    fn delivery_id(value: &str) -> DeliveryId {
        DeliveryId::parse(value).expect("test delivery identifier is valid")
    }

    fn pull_request_number(value: i64) -> PullRequestNumber {
        PullRequestNumber::new(value).expect("test pull-request number is positive")
    }

    fn sqlite_spans(captured: &CapturedSpans) -> Vec<&Span> {
        captured
            .spans
            .iter()
            .filter(|span| span.name == "sqlite.query")
            .collect()
    }

    fn sqlite_span<'spans>(captured: &'spans CapturedSpans, operation: &str) -> &'spans Span {
        let matches = sqlite_spans(captured)
            .into_iter()
            .filter(|span| string_attribute(span, "db.operation.name") == Some(operation))
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1, "sqlite span count for {operation}");
        matches[0]
    }

    fn assert_bounded_sqlite_span(span: &Span, operation: &str, outcome: &str) {
        assert_eq!(span.name, "sqlite.query");
        assert_attribute(span, "db.system.name", "sqlite");
        assert_attribute(span, "db.operation.name", operation);
        assert_attribute(span, "ghe.operation.outcome", outcome);

        let mut keys = span
            .attributes
            .iter()
            .map(|attribute| attribute.key.as_str())
            .collect::<Vec<_>>();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "db.operation.name",
                "db.system.name",
                "ghe.operation.outcome",
            ]
        );
        assert!(!keys.iter().any(|key| key.contains("statement")));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn store_operations_emit_one_bounded_sqlite_span_each() {
        let fixture = SqliteTraceFixture::new().await;

        let created = fixture
            .traced(fixture.repository_store.create(
                name("Owner/Repository"),
                secret("sqlite-secret-must-not-appear"),
                true,
            ))
            .await
            .expect("repository is created");
        fixture
            .traced(fixture.repository_store.count())
            .await
            .expect("repositories are counted");
        fixture
            .traced(fixture.repository_store.list())
            .await
            .expect("repositories are listed");
        fixture
            .traced(
                fixture
                    .repository_store
                    .authentication_secret(&name("owner/repository")),
            )
            .await
            .expect("authentication secret is loaded");
        fixture
            .traced(fixture.repository_store.get(created.id()))
            .await
            .expect("repository is loaded");
        fixture
            .traced(
                fixture
                    .repository_store
                    .update(created.id(), RepositoryMutation::new().with_enabled(false)),
            )
            .await
            .expect("repository is updated");

        fixture
            .traced(
                fixture
                    .delivery_store
                    .claim(&delivery_id("550e8400-e29b-41d4-a716-446655440300")),
            )
            .await
            .expect("delivery is claimed");
        fixture
            .traced(
                fixture
                    .delivery_store
                    .prune_batch(OffsetDateTime::now_utc() + Duration::seconds(1)),
            )
            .await
            .expect("delivery claims are pruned");

        let enqueued_at = queue_timestamp(ENQUEUED_AT);
        let completed_at = queue_timestamp(COMPLETED_AT);
        let pull_request_number = pull_request_number(42);
        fixture
            .traced(fixture.merge_queue_store.enqueue(
                created.id(),
                pull_request_number,
                &enqueued_at,
            ))
            .await
            .expect("queue attempt is enqueued");
        fixture
            .traced(fixture.merge_queue_store.complete(
                created.id(),
                pull_request_number,
                &QueueCompletion::pull_request_merged(completed_at),
            ))
            .await
            .expect("queue attempt is completed");
        fixture
            .traced(
                fixture.merge_queue_store.prune_completed_batch(
                    OffsetDateTime::parse(
                        "2026-08-05T10:06:00Z",
                        &time::format_description::well_known::Rfc3339,
                    )
                    .expect("cutoff timestamp is valid"),
                ),
            )
            .await
            .expect("queue attempts are pruned");

        fixture
            .traced(fixture.repository_store.delete(created.id()))
            .await
            .expect("repository is deleted");

        let captured = fixture.force_flush();
        let expected_operations = [
            "repository.create",
            "repository.count",
            "repository.list",
            "repository.authenticate",
            "repository.get",
            "repository.update",
            "delivery.claim",
            "delivery.prune",
            "merge_queue.enqueue",
            "merge_queue.complete",
            "merge_queue.prune",
            "repository.delete",
        ];
        assert_eq!(sqlite_spans(&captured).len(), expected_operations.len());
        for operation in expected_operations {
            assert_bounded_sqlite_span(sqlite_span(&captured, operation), operation, "success");
        }
        for forbidden in [
            "sqlite-secret-must-not-appear",
            fixture.database_path.as_str(),
        ] {
            captured.assert_absent(forbidden);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sqlite_errors_mark_failure_without_database_details() {
        let fixture = SqliteTraceFixture::new().await;
        sqlx::query(
            "CREATE TRIGGER repository_private_trigger BEFORE INSERT ON repositories \
             BEGIN SELECT RAISE(FAIL, 'private_table_detail_must_not_appear'); END",
        )
        .execute(&fixture.pool)
        .await
        .expect("failure trigger is installed");

        let result = fixture
            .traced(fixture.repository_store.create(
                name("owner/failure"),
                secret("failure-secret-must-not-appear"),
                true,
            ))
            .await;
        assert!(matches!(result, Err(RepositoryStoreError::Internal(_))));

        let captured = fixture.force_flush();
        let span = sqlite_span(&captured, "repository.create");
        assert_bounded_sqlite_span(span, "repository.create", "failure");
        assert_eq!(
            span.status.as_ref().map(|status| status.code),
            Some(opentelemetry_proto::tonic::trace::v1::status::StatusCode::Error as i32)
        );
        for forbidden in [
            "private_table_detail_must_not_appear",
            "repository_private_trigger",
            "failure-secret-must-not-appear",
            fixture.database_path.as_str(),
        ] {
            captured.assert_absent(forbidden);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_routes_emit_http_roots_and_write_children() {
    let fixture = RepositoryTraceFixture::new().await;

    let created = fixture
        .authorized_json(
            Method::POST,
            "/api/v1/repositories?raw-secret=must-not-appear",
            serde_json::json!({
                "full_name": " Owner/Repository ",
                "webhook_secret": "create-secret-must-not-appear"
            }),
        )
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let created_body = response_json(created).await;
    let repository_id = created_body["id"].as_i64().expect("created id is numeric");

    let listed = fixture
        .request(
            Method::GET,
            "/api/v1/repositories",
            Some("Bearer independent-admin-token"),
            None,
            Body::empty(),
        )
        .await;
    assert_eq!(listed.status(), StatusCode::OK);
    drop(listed);

    let updated = fixture
        .authorized_json(
            Method::PATCH,
            &format!("/api/v1/repositories/{repository_id}"),
            serde_json::json!({
                "full_name": "owner/renamed",
                "enabled": false
            }),
        )
        .await;
    assert_eq!(updated.status(), StatusCode::OK);
    drop(updated);

    let loaded = fixture
        .request(
            Method::GET,
            &format!("/api/v1/repositories/{repository_id}"),
            Some("Bearer independent-admin-token"),
            None,
            Body::empty(),
        )
        .await;
    assert_eq!(loaded.status(), StatusCode::OK);
    drop(loaded);

    let deleted = fixture
        .request(
            Method::DELETE,
            &format!("/api/v1/repositories/{repository_id}"),
            Some("Bearer independent-admin-token"),
            None,
            Body::empty(),
        )
        .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    drop(deleted);

    let captured = fixture.force_flush();
    let create_request = captured.http_request("POST", "/api/v1/repositories", 201);
    assert_attribute(create_request, "ghe.http.result", "success");
    let create_write = captured.child_named(create_request, "config.repository.write");
    assert_attribute(create_write, "ghe.config.operation", "create");
    assert_attribute(create_write, "github.repository.name", "owner/repository");
    assert_i64_attribute(create_write, "github.repository.id", repository_id);
    assert_attribute(create_write, "ghe.operation.outcome", "success");

    let list_request = captured.http_request("GET", "/api/v1/repositories", 200);
    assert_eq!(
        captured.child_count(list_request, "config.repository.write"),
        0
    );

    let update_request = captured.http_request("PATCH", "/api/v1/repositories/{id}", 200);
    let update_write = captured.child_named(update_request, "config.repository.write");
    assert_attribute(update_write, "ghe.config.operation", "update");
    assert_attribute(update_write, "github.repository.name", "owner/renamed");
    assert_i64_attribute(update_write, "github.repository.id", repository_id);
    assert_attribute(update_write, "ghe.operation.outcome", "success");

    let get_request = captured.http_request("GET", "/api/v1/repositories/{id}", 200);
    assert_eq!(
        captured.child_count(get_request, "config.repository.write"),
        0
    );

    let delete_request = captured.http_request("DELETE", "/api/v1/repositories/{id}", 204);
    let delete_write = captured.child_named(delete_request, "config.repository.write");
    assert_attribute(delete_write, "ghe.config.operation", "delete");
    assert_i64_attribute(delete_write, "github.repository.id", repository_id);
    assert_attribute(delete_write, "ghe.operation.outcome", "success");

    captured.assert_absent("raw-secret=must-not-appear");
    captured.assert_absent("create-secret-must-not-appear");
    captured.assert_absent("Bearer independent-admin-token");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_error_routes_emit_redacted_http_roots_without_unapproved_children() {
    let fixture = RepositoryTraceFixture::new().await;

    let malformed = fixture
        .request(
            Method::POST,
            "/api/v1/repositories?query-secret=must-not-appear",
            Some("Bearer independent-admin-token"),
            Some("application/json"),
            Body::from(r#"{"full_name":"owner/bad","webhook_secret":"bad-secret""#),
        )
        .await;
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    drop(malformed);

    let unauthorized = fixture
        .request(
            Method::GET,
            "/api/v1/repositories",
            Some("Bearer leaked-admin-token"),
            None,
            Body::empty(),
        )
        .await;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    drop(unauthorized);

    let unknown = fixture
        .request(
            Method::GET,
            "/missing/path?unknown-secret=must-not-appear",
            Some("Bearer unknown-route-token"),
            None,
            Body::empty(),
        )
        .await;
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    drop(unknown);

    let captured = fixture.force_flush();
    let malformed_request = captured.http_request("POST", "/api/v1/repositories", 400);
    assert_attribute(malformed_request, "ghe.http.result", "client_error");
    assert_eq!(
        captured.child_count(malformed_request, "config.repository.write"),
        0
    );
    let unauthorized_request = captured.http_request("GET", "/api/v1/repositories", 401);
    assert_attribute(unauthorized_request, "ghe.http.result", "client_error");
    assert_eq!(
        captured.child_count(unauthorized_request, "config.repository.write"),
        0
    );
    let unknown_request = captured.http_request("GET", "unmatched", 404);
    assert_attribute(unknown_request, "ghe.http.result", "client_error");
    assert_eq!(
        captured.child_count(unknown_request, "config.repository.write"),
        0
    );

    captured.assert_absent("query-secret=must-not-appear");
    captured.assert_absent("bad-secret");
    captured.assert_absent("Bearer leaked-admin-token");
    captured.assert_absent("unknown-secret=must-not-appear");
    captured.assert_absent("Bearer unknown-route-token");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn repository_store_failure_marks_http_and_write_spans_without_error_text() {
    let fixture = RepositoryTraceFixture::new().await;
    fixture.pool.close().await;

    let failed = fixture
        .authorized_json(
            Method::POST,
            "/api/v1/repositories",
            serde_json::json!({
                "full_name": "owner/failure",
                "webhook_secret": "failure-secret-must-not-appear"
            }),
        )
        .await;
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    drop(failed);

    let captured = fixture.force_flush();
    let request = captured.http_request("POST", "/api/v1/repositories", 500);
    assert_attribute(request, "ghe.http.result", "server_error");
    let write = captured.child_named(request, "config.repository.write");
    assert_attribute(write, "ghe.config.operation", "create");
    assert_attribute(write, "github.repository.name", "owner/failure");
    assert_attribute(write, "ghe.operation.outcome", "failure");
    let sqlite = captured.child_named(write, "sqlite.query");
    assert_attribute(sqlite, "db.system.name", "sqlite");
    assert_attribute(sqlite, "db.operation.name", "repository.create");
    assert_attribute(sqlite, "ghe.operation.outcome", "failure");
    assert_eq!(
        sqlite.status.as_ref().map(|status| status.code),
        Some(opentelemetry_proto::tonic::trace::v1::status::StatusCode::Error as i32)
    );
    captured.assert_absent("failure-secret-must-not-appear");
    captured.assert_absent("pool closed");
    captured.assert_absent("repository-trace.db");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_successes_emit_bounded_hierarchy_and_span_only_identifiers() {
    let fixture = WebhookTraceFixture::new().await;
    let merge_group_delivery = "550e8400-e29b-41d4-a716-446655440201";
    let enqueue_delivery = "550e8400-e29b-41d4-a716-446655440202";
    let dequeue_delivery = "550e8400-e29b-41d4-a716-446655440203";
    let malformed_sha_delivery = "550e8400-e29b-41d4-a716-446655440204";
    let non_string_pull_request_sha_delivery = "550e8400-e29b-41d4-a716-446655440205";
    let non_string_merge_group_sha_delivery = "550e8400-e29b-41d4-a716-446655440206";
    let merge_group_body = format!(
        r#"{{"action":"checks_requested","merge_group":{{"head_sha":"{WEBHOOK_SHA_64}"}},"repository":{{"full_name":"{WEBHOOK_REPOSITORY}"}}}}"#
    );
    let enqueue_body = format!(
        r#"{{"action":"enqueued","pull_request":{{"number":42,"updated_at":"2026-08-05T10:00:00Z","head":{{"sha":"{WEBHOOK_SHA_40}"}}}},"repository":{{"full_name":"{WEBHOOK_REPOSITORY}"}}}}"#
    );
    let dequeue_body = format!(
        r#"{{"action":"dequeued","reason":"raw-reason-must-not-appear","pull_request":{{"number":42,"updated_at":"2026-08-05T10:02:00Z","head":{{"sha":"{WEBHOOK_SHA_40}"}}}},"repository":{{"full_name":"{WEBHOOK_REPOSITORY}"}}}}"#
    );
    let malformed_sha_body = format!(
        r#"{{"action":"checks_requested","merge_group":{{"head_sha":"malformed-sha-must-not-appear"}},"repository":{{"full_name":"{WEBHOOK_REPOSITORY}"}}}}"#
    );
    let non_string_pull_request_sha_body = format!(
        r#"{{"action":"enqueued","pull_request":{{"number":44,"updated_at":"2026-08-05T10:04:00Z","head":{{"sha":42}}}},"repository":{{"full_name":"{WEBHOOK_REPOSITORY}"}}}}"#
    );
    let non_string_merge_group_sha_body = format!(
        r#"{{"action":"checks_requested","merge_group":{{"head_sha":{{"unexpected":"value"}}}},"repository":{{"full_name":"{WEBHOOK_REPOSITORY}"}}}}"#
    );

    for (body, event_type, delivery_id) in [
        (
            merge_group_body.as_bytes(),
            "merge_group",
            merge_group_delivery,
        ),
        (enqueue_body.as_bytes(), "pull_request", enqueue_delivery),
        (dequeue_body.as_bytes(), "pull_request", dequeue_delivery),
        (
            malformed_sha_body.as_bytes(),
            "merge_group",
            malformed_sha_delivery,
        ),
        (
            non_string_pull_request_sha_body.as_bytes(),
            "pull_request",
            non_string_pull_request_sha_delivery,
        ),
        (
            non_string_merge_group_sha_body.as_bytes(),
            "merge_group",
            non_string_merge_group_sha_delivery,
        ),
    ] {
        let response = fixture
            .webhook(body, event_type, delivery_id, WEBHOOK_SECRET)
            .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
    let repository_id: i64 = sqlx::query_scalar("SELECT id FROM repositories")
        .fetch_one(&fixture.pool)
        .await
        .expect("repository identifier is readable");
    let exposition = fixture.metrics_text().await;
    let captured = fixture.force_flush();

    let merge_request = captured.webhook_request_for_delivery(merge_group_delivery);
    let merge_authenticate = captured.child_named(merge_request, "github.webhook.authenticate");
    let merge_process = captured.child_named(merge_request, "github.webhook.process");
    for span in [merge_authenticate, merge_process] {
        assert_attribute(span, "github.repository.name", WEBHOOK_REPOSITORY);
        assert_i64_attribute(span, "github.repository.id", repository_id);
        assert_attribute(span, "github.delivery.id", merge_group_delivery);
        assert_attribute(span, "ghe.operation.outcome", "success");
    }
    assert_attribute(merge_process, "ghe.webhook.event_type", "merge_group");
    assert_attribute(merge_process, "ghe.webhook.action", "checks_requested");
    let claim = captured.descendant_named(merge_process, "sqlite.query", "delivery.claim");
    assert_attribute(claim, "db.operation.name", "delivery.claim");
    let merge_update =
        captured.descendant_named(merge_process, "merge_queue.update", "merge_group");
    assert_attribute(merge_update, "ghe.merge_group.action", "checks_requested");
    assert_attribute(merge_update, "ghe.merge_group.reason", "none");
    assert_attribute(
        merge_update,
        "github.commit.sha",
        &WEBHOOK_SHA_64.to_ascii_lowercase(),
    );
    assert_attribute(merge_update, "ghe.operation.outcome", "success");

    let enqueue_request = captured.webhook_request_for_delivery(enqueue_delivery);
    let enqueue_process = captured.child_named(enqueue_request, "github.webhook.process");
    let enqueue_update =
        captured.descendant_named(enqueue_process, "merge_queue.update", "pull_request");
    assert_i64_attribute(enqueue_update, "github.pull_request.number", 42);
    assert_attribute(enqueue_update, "github.delivery.id", enqueue_delivery);
    assert_attribute(
        enqueue_update,
        "github.commit.sha",
        &WEBHOOK_SHA_40.to_ascii_lowercase(),
    );
    assert_eq!(string_attribute(enqueue_update, "ghe.queue.outcome"), None);
    assert_eq!(string_attribute(enqueue_update, "ghe.queue.reason"), None);
    assert_attribute(enqueue_update, "ghe.operation.outcome", "success");

    let dequeue_request = captured.webhook_request_for_delivery(dequeue_delivery);
    let dequeue_process = captured.child_named(dequeue_request, "github.webhook.process");
    let dequeue_update =
        captured.descendant_named(dequeue_process, "merge_queue.update", "pull_request");
    assert_i64_attribute(dequeue_update, "github.repository.id", repository_id);
    assert_attribute(dequeue_update, "github.repository.name", WEBHOOK_REPOSITORY);
    assert_attribute(dequeue_update, "github.delivery.id", dequeue_delivery);
    assert_i64_attribute(dequeue_update, "github.pull_request.number", 42);
    assert_attribute(
        dequeue_update,
        "github.commit.sha",
        &WEBHOOK_SHA_40.to_ascii_lowercase(),
    );
    assert_attribute(dequeue_update, "ghe.queue.outcome", "unknown");
    assert_attribute(dequeue_update, "ghe.queue.reason", "unclassified_dequeue");
    assert_attribute(dequeue_update, "ghe.operation.outcome", "success");

    let malformed_sha_request = captured.webhook_request_for_delivery(malformed_sha_delivery);
    let malformed_sha_process =
        captured.child_named(malformed_sha_request, "github.webhook.process");
    let malformed_sha_update =
        captured.descendant_named(malformed_sha_process, "merge_queue.update", "merge_group");
    assert_eq!(
        string_attribute(malformed_sha_update, "github.commit.sha"),
        None
    );

    let non_string_pull_request_sha_request =
        captured.webhook_request_for_delivery(non_string_pull_request_sha_delivery);
    let non_string_pull_request_sha_process = captured.child_named(
        non_string_pull_request_sha_request,
        "github.webhook.process",
    );
    let non_string_pull_request_sha_update = captured.descendant_named(
        non_string_pull_request_sha_process,
        "merge_queue.update",
        "pull_request",
    );
    assert_eq!(
        string_attribute(non_string_pull_request_sha_update, "github.commit.sha"),
        None
    );

    let non_string_merge_group_sha_request =
        captured.webhook_request_for_delivery(non_string_merge_group_sha_delivery);
    let non_string_merge_group_sha_process =
        captured.child_named(non_string_merge_group_sha_request, "github.webhook.process");
    let non_string_merge_group_sha_update = captured.descendant_named(
        non_string_merge_group_sha_process,
        "merge_queue.update",
        "merge_group",
    );
    assert_eq!(
        string_attribute(non_string_merge_group_sha_update, "github.commit.sha"),
        None
    );

    let output = fixture.output.text();
    let normalized_sha_40 = WEBHOOK_SHA_40.to_ascii_lowercase();
    let normalized_sha_64 = WEBHOOK_SHA_64.to_ascii_lowercase();
    for identifier in [
        WEBHOOK_REPOSITORY,
        merge_group_delivery,
        enqueue_delivery,
        dequeue_delivery,
        malformed_sha_delivery,
        non_string_pull_request_sha_delivery,
        non_string_merge_group_sha_delivery,
        WEBHOOK_SHA_40,
        WEBHOOK_SHA_64,
        normalized_sha_40.as_str(),
        normalized_sha_64.as_str(),
    ] {
        captured.assert_logs_absent(identifier);
        assert!(!output.contains(identifier));
        assert!(!exposition.contains(identifier));
    }
    for forbidden in [
        WEBHOOK_SECRET,
        "raw-reason-must-not-appear",
        "malformed-sha-must-not-appear",
        "sha256=",
    ] {
        captured.assert_absent(forbidden);
        captured.assert_logs_absent(forbidden);
        assert!(!output.contains(forbidden));
        assert!(!exposition.contains(forbidden));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_failures_mark_authentication_and_processing_without_raw_error_data() {
    let fixture = WebhookTraceFixture::new().await;
    let unauthorized_delivery = "550e8400-e29b-41d4-a716-446655440211";
    let malformed_delivery = "550e8400-e29b-41d4-a716-446655440212";
    let unavailable_delivery = "550e8400-e29b-41d4-a716-446655440213";
    let normal_body =
        format!(r#"{{"action":"opened","repository":{{"full_name":"{WEBHOOK_REPOSITORY}"}}}}"#);
    let malformed_body = format!(
        r#"{{"action":{{"raw":"action-must-not-appear"}},"repository":{{"full_name":"{WEBHOOK_REPOSITORY}"}}}}"#
    );

    let unauthorized = fixture
        .webhook(
            normal_body.as_bytes(),
            "pull_request",
            unauthorized_delivery,
            "wrong-secret-must-not-appear",
        )
        .await;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    drop(unauthorized);
    let malformed = fixture
        .webhook(
            malformed_body.as_bytes(),
            "pull_request",
            malformed_delivery,
            WEBHOOK_SECRET,
        )
        .await;
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    drop(malformed);
    sqlx::query("DROP TABLE processed_deliveries")
        .execute(&fixture.pool)
        .await
        .expect("delivery table is removed");
    let unavailable = fixture
        .webhook(
            normal_body.as_bytes(),
            "pull_request",
            unavailable_delivery,
            WEBHOOK_SECRET,
        )
        .await;
    assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE);
    drop(unavailable);
    let captured = fixture.force_flush();

    let unauthorized_request = captured.webhook_request_for_delivery(unauthorized_delivery);
    let unauthorized_authenticate =
        captured.child_named(unauthorized_request, "github.webhook.authenticate");
    assert_attribute(
        unauthorized_authenticate,
        "ghe.operation.outcome",
        "failure",
    );
    assert_eq!(
        captured.child_count(unauthorized_request, "github.webhook.process"),
        0
    );

    let malformed_request = captured.webhook_request_for_delivery(malformed_delivery);
    let malformed_authenticate =
        captured.child_named(malformed_request, "github.webhook.authenticate");
    assert_attribute(malformed_authenticate, "ghe.operation.outcome", "success");
    let malformed_process = captured.child_named(malformed_request, "github.webhook.process");
    assert_attribute(malformed_process, "ghe.operation.outcome", "failure");
    assert_eq!(
        captured.descendant_count(malformed_process, "merge_queue.update"),
        0
    );

    let unavailable_request = captured.webhook_request_for_delivery(unavailable_delivery);
    let unavailable_process = captured.child_named(unavailable_request, "github.webhook.process");
    assert_attribute(unavailable_process, "ghe.operation.outcome", "failure");
    let failed_claim =
        captured.descendant_named(unavailable_process, "sqlite.query", "delivery.claim");
    assert_attribute(failed_claim, "ghe.operation.outcome", "failure");

    for forbidden in [
        "wrong-secret-must-not-appear",
        "action-must-not-appear",
        "processed_deliveries",
    ] {
        captured.assert_absent(forbidden);
        captured.assert_logs_absent(forbidden);
        assert!(!fixture.output.text().contains(forbidden));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_queue_failure_is_bounded_and_duplicate_has_no_second_update() {
    let fixture = WebhookTraceFixture::new().await;
    let enqueue_delivery = "550e8400-e29b-41d4-a716-446655440221";
    let dequeue_delivery = "550e8400-e29b-41d4-a716-446655440222";
    let enqueue_body = format!(
        r#"{{"action":"enqueued","pull_request":{{"number":77,"updated_at":"2026-08-05T10:00:00Z","head":{{"sha":"{WEBHOOK_SHA_40}"}}}},"repository":{{"full_name":"{WEBHOOK_REPOSITORY}"}}}}"#
    );
    let dequeue_body = format!(
        r#"{{"action":"dequeued","reason":"queue-raw-reason-must-not-appear","pull_request":{{"number":77,"updated_at":"2026-08-05T10:01:00Z","head":{{"sha":"{WEBHOOK_SHA_40}"}}}},"repository":{{"full_name":"{WEBHOOK_REPOSITORY}"}}}}"#
    );
    let enqueued = fixture
        .webhook(
            enqueue_body.as_bytes(),
            "pull_request",
            enqueue_delivery,
            WEBHOOK_SECRET,
        )
        .await;
    assert_eq!(enqueued.status(), StatusCode::NO_CONTENT);
    drop(enqueued);
    fixture.flush();
    sqlx::query(
        "CREATE TRIGGER reject_trace_queue_completion BEFORE UPDATE ON merge_queue_attempts \
         BEGIN SELECT RAISE(ABORT, 'queue-store-detail-must-not-appear'); END",
    )
    .execute(&fixture.pool)
    .await
    .expect("queue failure trigger is installed");

    for _ in 0..2 {
        let response = fixture
            .webhook(
                dequeue_body.as_bytes(),
                "pull_request",
                dequeue_delivery,
                WEBHOOK_SECRET,
            )
            .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        drop(response);
        fixture.flush();
    }
    let exposition = fixture.metrics_text().await;
    let captured = fixture.force_flush();

    let matching_updates: Vec<&Span> = captured
        .spans
        .iter()
        .filter(|span| {
            span.name == "merge_queue.update"
                && string_attribute(span, "github.delivery.id") == Some(dequeue_delivery)
        })
        .collect();
    assert_eq!(matching_updates.len(), 1);
    let update = matching_updates[0];
    assert_attribute(update, "ghe.queue.outcome", "unknown");
    assert_attribute(update, "ghe.queue.reason", "unclassified_dequeue");
    assert_attribute(update, "ghe.operation.outcome", "failure");
    assert_eq!(
        update.status.as_ref().map(|status| status.code),
        Some(opentelemetry_proto::tonic::trace::v1::status::StatusCode::Error as i32)
    );
    let failure_event = update
        .events
        .iter()
        .find(|event| event.name == "operation.failure")
        .expect("queue failure event is exported");
    assert_eq!(
        event_string_attribute(failure_event, "ghe.failure.reason"),
        Some("queue_state")
    );
    let duplicate_processes = captured
        .spans
        .iter()
        .filter(|span| {
            span.name == "github.webhook.process"
                && string_attribute(span, "github.delivery.id") == Some(dequeue_delivery)
                && string_attribute(span, "ghe.operation.outcome") == Some("duplicate")
        })
        .count();
    assert_eq!(duplicate_processes, 1);
    assert!(exposition.contains("github_webhook_duplicates_total 1"));
    assert!(
        exposition.contains("github_webhook_processing_failures_total{stage=\"queue_state\"} 1")
    );
    for forbidden in [
        "queue-raw-reason-must-not-appear",
        "queue-store-detail-must-not-appear",
        WEBHOOK_SECRET,
    ] {
        captured.assert_absent(forbidden);
        captured.assert_logs_absent(forbidden);
        assert!(!fixture.output.text().contains(forbidden));
        assert!(!exposition.contains(forbidden));
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integrated_core_trace_privacy() {
    use crate::retention::{run_retention, RetentionConfig};

    let fixture = WebhookTraceFixture::new().await;
    sqlx::query("UPDATE repositories SET id = ? WHERE full_name = ?")
        .bind(PRIVACY_REPOSITORY_SEQUENCE)
        .bind(WEBHOOK_REPOSITORY)
        .execute(&fixture.pool)
        .await
        .expect("fixture repository identifier is moved to a distinctive range");

    let created = fixture
        .authorized_json(
            Method::POST,
            "/api/v1/repositories",
            serde_json::json!({
                "full_name": PRIVACY_REPOSITORY,
                "webhook_secret": SECRET
            }),
        )
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let created_body = response_json(created).await;
    let repository_id = created_body["id"]
        .as_i64()
        .expect("privacy repository identifier is numeric");
    assert_eq!(repository_id, PRIVACY_REPOSITORY_SEQUENCE + 1);

    let unauthorized_admin = fixture
        .request(
            Method::GET,
            "/api/v1/repositories",
            Some(AUTHORIZATION),
            None,
            Body::empty(),
        )
        .await;
    assert_eq!(unauthorized_admin.status(), StatusCode::UNAUTHORIZED);
    drop(unauthorized_admin);
    let unmatched_uri = format!("{RAW_UNMATCHED_PATH}?target={RAW_URL}");
    let unmatched = fixture
        .request(Method::GET, &unmatched_uri, None, None, Body::empty())
        .await;
    assert_eq!(unmatched.status(), StatusCode::NOT_FOUND);
    drop(unmatched);

    let merge_group_body = serde_json::to_vec(&serde_json::json!({
        "action": "destroyed",
        "reason": "dequeued",
        "merge_group": {"head_sha": PRIVACY_SHA},
        "repository": {
            "full_name": PRIVACY_REPOSITORY,
            "html_url": RAW_URL
        },
        "sender": {"login": ACTOR},
        "command": COMMAND
    }))
    .expect("merge-group body serializes");
    let merge_group = fixture
        .webhook(
            &merge_group_body,
            "merge_group",
            PRIVACY_MERGE_GROUP_DELIVERY,
            SECRET,
        )
        .await;
    assert_eq!(merge_group.status(), StatusCode::NO_CONTENT);
    drop(merge_group);
    fixture.flush();

    let enqueue_body = serde_json::to_vec(&serde_json::json!({
        "action": "enqueued",
        "pull_request": {
            "number": PRIVACY_PR_NUMBER,
            "updated_at": "2026-08-05T10:00:00Z",
            "head": {"sha": PRIVACY_SHA}
        },
        "repository": {"full_name": PRIVACY_REPOSITORY},
        "sender": {"login": ACTOR},
        "command": COMMAND,
        "url": RAW_URL
    }))
    .expect("enqueue body serializes");
    let enqueued = fixture
        .webhook(
            &enqueue_body,
            "pull_request",
            PRIVACY_ENQUEUE_DELIVERY,
            SECRET,
        )
        .await;
    assert_eq!(enqueued.status(), StatusCode::NO_CONTENT);
    drop(enqueued);
    fixture.flush();

    let dequeue_body = serde_json::to_vec(&serde_json::json!({
        "action": "dequeued",
        "reason": RAW_REASON,
        "pull_request": {
            "number": PRIVACY_PR_NUMBER,
            "updated_at": "2026-08-05T10:01:00Z",
            "head": {"sha": PRIVACY_SHA}
        },
        "repository": {"full_name": PRIVACY_REPOSITORY},
        "sender": {"login": ACTOR},
        "command": COMMAND,
        "url": RAW_URL
    }))
    .expect("dequeue body serializes");
    let dequeued = fixture
        .webhook(
            &dequeue_body,
            "pull_request",
            PRIVACY_DEQUEUE_DELIVERY,
            SECRET,
        )
        .await;
    assert_eq!(dequeued.status(), StatusCode::NO_CONTENT);
    drop(dequeued);
    fixture.flush();
    let duplicate = fixture
        .webhook(
            &dequeue_body,
            "pull_request",
            PRIVACY_DEQUEUE_DELIVERY,
            SECRET,
        )
        .await;
    assert_eq!(duplicate.status(), StatusCode::NO_CONTENT);
    drop(duplicate);
    fixture.flush();

    let auth_failure_body = serde_json::to_vec(&serde_json::json!({
        "action": "opened",
        "repository": {"full_name": PRIVACY_REPOSITORY},
        "sender": {"login": ACTOR},
        "command": COMMAND,
        "url": RAW_URL
    }))
    .expect("authentication failure body serializes");
    let auth_failure = fixture
        .webhook_with_signature(
            &auth_failure_body,
            "pull_request",
            PRIVACY_AUTH_FAILURE_DELIVERY,
            SIGNATURE,
        )
        .await;
    assert_eq!(auth_failure.status(), StatusCode::UNAUTHORIZED);
    drop(auth_failure);
    fixture.flush();

    let process_failure_body = serde_json::to_vec(&serde_json::json!({
        "action": {"raw_command": COMMAND},
        "reason": RAW_REASON,
        "repository": {
            "full_name": PRIVACY_REPOSITORY,
            "html_url": RAW_URL
        },
        "sender": {"login": ACTOR}
    }))
    .expect("processing failure body serializes");
    let process_failure = fixture
        .webhook(
            &process_failure_body,
            "pull_request",
            PRIVACY_PROCESS_FAILURE_DELIVERY,
            SECRET,
        )
        .await;
    assert_eq!(process_failure.status(), StatusCode::BAD_REQUEST);
    drop(process_failure);
    fixture.flush();

    sqlx::query("INSERT INTO processed_deliveries (delivery_id, received_at) VALUES (?, ?)")
        .bind(PRIVACY_RETENTION_DELIVERY)
        .bind("2020-01-01T00:00:00.000Z")
        .execute(&fixture.pool)
        .await
        .expect("expired delivery is inserted");
    sqlx::query(
        "INSERT INTO merge_queue_attempts \
             (repository_id, pull_request_number, enqueued_at, completed_at, outcome, reason_code) \
         VALUES (?, ?, '2020-01-01T00:00:00.000Z', '2020-01-02T00:00:00.000Z', \
                 'unknown', 'unclassified_dequeue')",
    )
    .bind(repository_id)
    .bind(PRIVACY_PR_NUMBER + 1)
    .execute(&fixture.pool)
    .await
    .expect("expired queue attempt is inserted");
    let retention_config = RetentionConfig::new(
        std::time::Duration::from_millis(25),
        std::time::Duration::from_secs(86_400),
        std::time::Duration::from_secs(90 * 86_400),
    )
    .expect("retention configuration is valid");
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let ambient_request = trace::operation_span(trace::Operation::HttpRequest);
    let retention_runner = tokio::spawn(
        run_retention(
            DeliveryStore::new(fixture.pool.clone()),
            MergeQueueStore::new(fixture.pool.clone()),
            retention_config,
            shutdown_receiver,
        )
        .instrument(ambient_request.clone())
        .with_subscriber(fixture.dispatch.clone()),
    );
    let retention_deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let retained_delivery: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM processed_deliveries WHERE delivery_id = ?")
                .bind(PRIVACY_RETENTION_DELIVERY)
                .fetch_one(&fixture.pool)
                .await
                .expect("retention delivery is countable");
        let retained_attempt: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM merge_queue_attempts \
             WHERE repository_id = ? AND pull_request_number = ?",
        )
        .bind(repository_id)
        .bind(PRIVACY_PR_NUMBER + 1)
        .fetch_one(&fixture.pool)
        .await
        .expect("retention queue attempt is countable");
        if retained_delivery == 0 && retained_attempt == 0 {
            break;
        }
        assert!(
            std::time::Instant::now() < retention_deadline,
            "integrated retention pass did not complete before the diagnostic deadline"
        );
        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
    }
    shutdown_sender
        .send(true)
        .expect("retention runner receives shutdown");
    retention_runner.await.expect("retention runner joins");
    drop(ambient_request);

    let exposition = fixture.metrics_text().await;
    let captured = fixture.force_flush();

    let create_request = captured.http_request("POST", "/api/v1/repositories", 201);
    let repository_write = captured.child_named(create_request, "config.repository.write");
    assert_attribute(
        repository_write,
        "github.repository.name",
        PRIVACY_REPOSITORY,
    );
    assert_i64_attribute(repository_write, "github.repository.id", repository_id);
    assert_eq!(
        captured.http_request("GET", "unmatched", 404).name,
        "http.request"
    );

    let merge_request = captured.webhook_request_for_delivery(PRIVACY_MERGE_GROUP_DELIVERY);
    let merge_process = captured.child_named(merge_request, "github.webhook.process");
    let merge_update =
        captured.descendant_named(merge_process, "merge_queue.update", "merge_group");
    assert_attribute(merge_update, "ghe.merge_group.action", "destroyed");
    assert_attribute(merge_update, "ghe.merge_group.reason", "dequeued");
    assert_attribute(merge_update, "github.commit.sha", PRIVACY_SHA);

    let dequeue_request = captured.webhook_request_for_delivery(PRIVACY_DEQUEUE_DELIVERY);
    let dequeue_process = captured.child_named(dequeue_request, "github.webhook.process");
    let dequeue_update =
        captured.descendant_named(dequeue_process, "merge_queue.update", "pull_request");
    assert_i64_attribute(
        dequeue_update,
        "github.pull_request.number",
        PRIVACY_PR_NUMBER,
    );
    assert_attribute(dequeue_update, "ghe.queue.outcome", "unknown");
    assert_attribute(dequeue_update, "ghe.queue.reason", "unclassified_dequeue");
    assert_attribute(dequeue_update, "ghe.operation.outcome", "success");
    assert_eq!(
        captured
            .spans
            .iter()
            .filter(|span| {
                span.name == "merge_queue.update"
                    && string_attribute(span, "github.delivery.id")
                        == Some(PRIVACY_DEQUEUE_DELIVERY)
            })
            .count(),
        1,
        "duplicate delivery must not emit a second queue update"
    );
    assert_eq!(
        captured
            .spans
            .iter()
            .filter(|span| {
                span.name == "github.webhook.process"
                    && string_attribute(span, "github.delivery.id")
                        == Some(PRIVACY_DEQUEUE_DELIVERY)
                    && string_attribute(span, "ghe.operation.outcome") == Some("duplicate")
            })
            .count(),
        1,
        "duplicate delivery has one bounded process span"
    );

    let auth_failure_request = captured.webhook_request_for_delivery(PRIVACY_AUTH_FAILURE_DELIVERY);
    let auth_failure_span =
        captured.child_named(auth_failure_request, "github.webhook.authenticate");
    assert_attribute(auth_failure_span, "ghe.operation.outcome", "failure");
    assert_eq!(
        captured.child_count(auth_failure_request, "github.webhook.process"),
        0
    );
    let process_failure_request =
        captured.webhook_request_for_delivery(PRIVACY_PROCESS_FAILURE_DELIVERY);
    let process_failure_span =
        captured.child_named(process_failure_request, "github.webhook.process");
    assert_attribute(process_failure_span, "ghe.operation.outcome", "failure");

    let retention_roots = captured
        .spans
        .iter()
        .filter(|span| span.name == "retention.run")
        .collect::<Vec<_>>();
    assert_eq!(retention_roots.len(), 1, "one retention pass is exported");
    let retention_root = retention_roots[0];
    assert!(
        retention_root.parent_span_id.is_empty(),
        "retention run remains an independent root"
    );
    assert_attribute(retention_root, "ghe.operation.outcome", "success");
    let retention_children = captured
        .spans
        .iter()
        .filter(|span| span.parent_span_id == retention_root.span_id)
        .map(|span| {
            (
                span.name.as_str(),
                string_attribute(span, "db.operation.name"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        retention_children.len(),
        2,
        "retention children: {retention_children:?}; spans: {:?}",
        captured
            .spans
            .iter()
            .map(|span| (
                span.name.as_str(),
                span.span_id.as_slice(),
                span.parent_span_id.as_slice(),
                string_attribute(span, "db.operation.name")
            ))
            .collect::<Vec<_>>()
    );

    for operation in [
        "http.request",
        "github.webhook.authenticate",
        "github.webhook.process",
        "config.repository.write",
        "sqlite.query",
        "merge_queue.update",
        "retention.run",
    ] {
        assert!(
            captured.spans.iter().any(|span| span.name == operation),
            "stable operation span {operation:?} is exported"
        );
    }
    for delivery_id in [
        PRIVACY_MERGE_GROUP_DELIVERY,
        PRIVACY_ENQUEUE_DELIVERY,
        PRIVACY_DEQUEUE_DELIVERY,
        PRIVACY_AUTH_FAILURE_DELIVERY,
        PRIVACY_PROCESS_FAILURE_DELIVERY,
    ] {
        assert!(
            captured
                .spans
                .iter()
                .any(|span| { string_attribute(span, "github.delivery.id") == Some(delivery_id) }),
            "approved delivery identifier {delivery_id:?} is present in traces"
        );
    }
    assert!(captured.spans.iter().any(|span| {
        string_attribute(span, "github.repository.name") == Some(PRIVACY_REPOSITORY)
            && i64_attribute(span, "github.repository.id") == Some(repository_id)
    }));
    assert!(captured.spans.iter().any(|span| {
        i64_attribute(span, "github.pull_request.number") == Some(PRIVACY_PR_NUMBER)
            && string_attribute(span, "github.commit.sha") == Some(PRIVACY_SHA)
    }));

    captured.assert_approved_attribute_keys();
    let stderr = fixture.output.text();
    let repository_id_text = repository_id.to_string();
    let pull_request_number_text = PRIVACY_PR_NUMBER.to_string();
    for approved_identifier in [
        PRIVACY_REPOSITORY,
        PRIVACY_MERGE_GROUP_DELIVERY,
        PRIVACY_ENQUEUE_DELIVERY,
        PRIVACY_DEQUEUE_DELIVERY,
        PRIVACY_AUTH_FAILURE_DELIVERY,
        PRIVACY_PROCESS_FAILURE_DELIVERY,
        PRIVACY_SHA,
        repository_id_text.as_str(),
        pull_request_number_text.as_str(),
    ] {
        captured.assert_logs_absent(approved_identifier);
        assert!(
            !stderr.contains(approved_identifier),
            "stderr must not contain span-only identifier {approved_identifier:?}"
        );
        assert!(
            !exposition.contains(approved_identifier),
            "Prometheus must not contain span-only identifier {approved_identifier:?}"
        );
    }
    for forbidden in [
        SECRET,
        SIGNATURE,
        AUTHORIZATION,
        ACTOR,
        COMMAND,
        RAW_REASON,
        RAW_URL,
        RAW_UNMATCHED_PATH,
    ] {
        captured.assert_absent(forbidden);
        captured.assert_logs_absent(forbidden);
        assert!(
            !stderr.contains(forbidden),
            "stderr must not contain forbidden value {forbidden:?}"
        );
        assert!(
            !exposition.contains(forbidden),
            "Prometheus must not contain forbidden value {forbidden:?}"
        );
    }
    assert!(exposition.contains("github_webhook_duplicates_total 1"));
}

mod retention {
    use std::{sync::Arc, time::Duration as StdDuration};

    use tracing_subscriber::fmt::MakeWriter;

    use super::*;
    use crate::retention::{run_retention, RetentionConfig};

    const RETENTION_TRACE_QUEUE_CAPACITY: usize = 128;

    struct RetentionTraceFixture {
        _otlp_guard: OtlpTestGuard,
        receiver: RunningReceiver,
        runtime: super::super::TelemetryRuntime,
        dispatch: Dispatch,
        pool: SqlitePool,
        delivery_store: DeliveryStore,
        merge_queue_store: MergeQueueStore,
        _directory: TempDir,
    }

    impl RetentionTraceFixture {
        async fn new() -> Self {
            Self::new_with_writer(io::sink).await
        }

        async fn new_with_writer<W>(writer: W) -> Self
        where
            W: for<'writer> MakeWriter<'writer> + Send + Sync + 'static,
        {
            let otlp_guard = otlp_test_lock().lock_owned().await;
            let receiver = RunningReceiver::start_released().await;
            let config = telemetry_config_with_queue_capacity(
                &receiver.endpoint(),
                RETENTION_TRACE_QUEUE_CAPACITY,
            );
            let (runtime, subscriber) =
                build_runtime("github_webhook_exporter=info", &config, writer)
                    .expect("telemetry runtime initializes");
            let dispatch = Dispatch::new(subscriber);
            let directory = tempfile::tempdir().expect("temporary directory is created");
            let database_path = directory.path().join("retention-trace.db");
            let pool = open_database(&database_path)
                .await
                .expect("test database opens and migrates");
            insert_repository(&pool).await;
            let delivery_store = DeliveryStore::new(pool.clone());
            let merge_queue_store = MergeQueueStore::new(pool.clone());

            Self {
                _otlp_guard: otlp_guard,
                receiver,
                runtime,
                dispatch,
                pool,
                delivery_store,
                merge_queue_store,
                _directory: directory,
            }
        }

        fn spawn_retention(
            &self,
            config: RetentionConfig,
            shutdown: watch::Receiver<bool>,
        ) -> tokio::task::JoinHandle<()> {
            let ambient_request = trace::operation_span(trace::Operation::HttpRequest);
            tokio::spawn(
                run_retention(
                    self.delivery_store.clone(),
                    self.merge_queue_store.clone(),
                    config,
                    shutdown,
                )
                .instrument(ambient_request)
                .with_subscriber(self.dispatch.clone()),
            )
        }

        fn force_flush(&self) -> CapturedSpans {
            tokio::task::block_in_place(|| self.runtime.force_flush().expect("providers flush"));
            let (traces, logs) = self.receiver.captured_requests();
            CapturedSpans::from_requests(traces, logs)
        }
    }

    #[derive(Clone)]
    struct ShutdownOnDeliveryCompleted {
        bytes: Arc<Mutex<Vec<u8>>>,
        shutdown: watch::Sender<bool>,
    }

    impl ShutdownOnDeliveryCompleted {
        fn new(shutdown: watch::Sender<bool>) -> Self {
            Self {
                bytes: Arc::new(Mutex::new(Vec::new())),
                shutdown,
            }
        }
    }

    struct ShutdownWriter {
        bytes: Arc<Mutex<Vec<u8>>>,
        shutdown: watch::Sender<bool>,
    }

    impl Write for ShutdownWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let mut bytes = self
                .bytes
                .lock()
                .map_err(|_| io::Error::other("captured logs lock was poisoned"))?;
            bytes.extend_from_slice(buffer);
            let text = String::from_utf8_lossy(&bytes);
            if text.contains("workload=\"delivery\"")
                && text.contains("outcome=\"completed\"")
                && !text.contains("workload=\"merge_queue\"")
            {
                let _ignored = self.shutdown.send(true);
            }
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for ShutdownOnDeliveryCompleted {
        type Writer = ShutdownWriter;

        fn make_writer(&'writer self) -> Self::Writer {
            ShutdownWriter {
                bytes: Arc::clone(&self.bytes),
                shutdown: self.shutdown.clone(),
            }
        }
    }

    async fn insert_repository(pool: &SqlitePool) {
        sqlx::query(
            "INSERT INTO repositories (id, full_name, webhook_secret_ciphertext, \
             webhook_secret_nonce, encryption_version, enabled, created_at, updated_at) \
             VALUES (1, 'owner/retention-trace', X'01', X'02', 1, 1, \
                     '2026-01-01T00:00:00.000Z', '2026-01-01T00:00:00.000Z')",
        )
        .execute(pool)
        .await
        .expect("repository fixture is inserted");
    }

    async fn insert_delivery(pool: &SqlitePool, delivery_id: &str) {
        sqlx::query("INSERT INTO processed_deliveries (delivery_id, received_at) VALUES (?, ?)")
            .bind(delivery_id)
            .bind("2020-01-01T00:00:00.000Z")
            .execute(pool)
            .await
            .expect("delivery fixture is inserted");
    }

    async fn insert_queue_attempt(pool: &SqlitePool, pull_request_number: i64) {
        sqlx::query(
            "INSERT INTO merge_queue_attempts \
                 (repository_id, pull_request_number, enqueued_at, completed_at, outcome, reason_code) \
             VALUES (1, ?, '2020-01-01T00:00:00.000Z', '2020-01-02T00:00:00.000Z', \
                     'unknown', 'unclassified_dequeue')",
        )
        .bind(pull_request_number)
        .execute(pool)
        .await
        .expect("queue attempt fixture is inserted");
    }

    async fn delivery_count(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM processed_deliveries")
            .fetch_one(pool)
            .await
            .expect("delivery claims are countable")
    }

    async fn queue_attempt_count(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM merge_queue_attempts")
            .fetch_one(pool)
            .await
            .expect("queue attempts are countable")
    }

    async fn wait_for_delivery_count(pool: &SqlitePool, expected: i64) {
        let deadline = std::time::Instant::now() + StdDuration::from_secs(5);
        while delivery_count(pool).await != expected {
            assert!(
                std::time::Instant::now() < deadline,
                "delivery retention did not reach the expected count"
            );
            tokio::time::sleep(StdDuration::from_millis(1)).await;
        }
    }

    async fn wait_for_queue_attempt_count(pool: &SqlitePool, expected: i64) {
        let deadline = std::time::Instant::now() + StdDuration::from_secs(5);
        while queue_attempt_count(pool).await != expected {
            assert!(
                std::time::Instant::now() < deadline,
                "queue retention did not reach the expected count"
            );
            tokio::time::sleep(StdDuration::from_millis(1)).await;
        }
    }

    fn standard_config() -> RetentionConfig {
        RetentionConfig::new(
            StdDuration::from_millis(250),
            StdDuration::from_secs(86_400),
            StdDuration::from_secs(90 * 86_400),
        )
        .expect("retention configuration is valid")
    }

    fn invalid_cutoff_config() -> RetentionConfig {
        RetentionConfig::new(
            StdDuration::from_millis(250),
            StdDuration::from_secs(100_000_000 * 86_400),
            StdDuration::from_secs(100_000_000 * 86_400),
        )
        .expect("retention configuration is valid")
    }

    async fn tick_once() {
        tokio::task::yield_now().await;
        tokio::time::sleep(StdDuration::from_millis(275)).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
    }

    async fn stop_runner(shutdown: watch::Sender<bool>, runner: tokio::task::JoinHandle<()>) {
        let _ignored = shutdown.send(true);
        runner.await.expect("retention runner joins");
    }

    fn assert_root_outcome_only(span: &Span, outcome: &str) {
        assert!(
            span.parent_span_id.is_empty(),
            "retention roots must not inherit ambient parents"
        );
        assert_attribute(span, "ghe.operation.outcome", outcome);
        let keys = span
            .attributes
            .iter()
            .map(|attribute| attribute.key.as_str())
            .collect::<Vec<_>>();
        assert_eq!(keys, ["ghe.operation.outcome"]);
    }

    fn assert_sqlite_child(captured: &CapturedSpans, root: &Span, operation: &str, outcome: &str) {
        let span = captured
            .children(root)
            .find(|span| {
                span.name == "sqlite.query"
                    && string_attribute(span, "db.operation.name") == Some(operation)
            })
            .unwrap_or_else(|| panic!("{operation} sqlite child span is exported"));
        assert_attribute(span, "ghe.operation.outcome", outcome);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn completed_pass_exports_independent_root_with_prune_children() {
        let fixture = RetentionTraceFixture::new().await;
        insert_delivery(&fixture.pool, "30000000-0000-4000-8000-000000000001").await;
        insert_queue_attempt(&fixture.pool, 3001).await;
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        let runner = fixture.spawn_retention(standard_config(), shutdown_receiver);

        tick_once().await;
        wait_for_delivery_count(&fixture.pool, 0).await;
        wait_for_queue_attempt_count(&fixture.pool, 0).await;
        stop_runner(shutdown_sender, runner).await;
        let captured = fixture.force_flush();

        let root = captured.one_named("retention.run");
        assert_root_outcome_only(root, "success");
        assert_sqlite_child(&captured, root, "delivery.prune", "success");
        assert_sqlite_child(&captured, root, "merge_queue.prune", "success");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_after_delivery_exports_cancelled_root_without_queue_child() {
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        let fixture = RetentionTraceFixture::new_with_writer(ShutdownOnDeliveryCompleted::new(
            shutdown_sender.clone(),
        ))
        .await;
        insert_delivery(&fixture.pool, "30000000-0000-4000-8000-000000000002").await;
        insert_queue_attempt(&fixture.pool, 3002).await;
        let runner = fixture.spawn_retention(standard_config(), shutdown_receiver);

        tick_once().await;
        wait_for_delivery_count(&fixture.pool, 0).await;
        tokio::time::timeout(StdDuration::from_secs(5), runner)
            .await
            .expect("retention runner stops after delivery cancellation")
            .expect("retention runner joins after cancellation");
        let captured = fixture.force_flush();

        let root = captured.one_named("retention.run");
        assert_root_outcome_only(root, "cancelled");
        assert_sqlite_child(&captured, root, "delivery.prune", "success");
        assert_eq!(
            captured
                .children(root)
                .filter(|span| {
                    span.name == "sqlite.query"
                        && string_attribute(span, "db.operation.name") == Some("merge_queue.prune")
                })
                .count(),
            0
        );
        assert_eq!(queue_attempt_count(&fixture.pool).await, 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_cutoff_exports_failure_root_without_cutoff_attributes() {
        let fixture = RetentionTraceFixture::new().await;
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        let runner = fixture.spawn_retention(invalid_cutoff_config(), shutdown_receiver);

        tick_once().await;
        stop_runner(shutdown_sender, runner).await;
        let captured = fixture.force_flush();

        let root = captured.one_named("retention.run");
        assert_root_outcome_only(root, "failure");
        assert_eq!(captured.child_count(root, "sqlite.query"), 0);
        captured.assert_absent("invalid_cutoff");
        captured.assert_absent("cutoff");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_store_failure_exports_failure_root_and_preserves_other_workload() {
        let fixture = RetentionTraceFixture::new().await;
        insert_queue_attempt(&fixture.pool, 3003).await;
        sqlx::query("DROP TABLE processed_deliveries")
            .execute(&fixture.pool)
            .await
            .expect("delivery table is removed");
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        let runner = fixture.spawn_retention(standard_config(), shutdown_receiver);

        tick_once().await;
        wait_for_queue_attempt_count(&fixture.pool, 0).await;
        stop_runner(shutdown_sender, runner).await;
        let captured = fixture.force_flush();

        let root = captured.one_named("retention.run");
        assert_root_outcome_only(root, "failure");
        assert_sqlite_child(&captured, root, "delivery.prune", "failure");
        assert_sqlite_child(&captured, root, "merge_queue.prune", "success");
        for forbidden in [
            "processed_deliveries",
            "no such table",
            "error_correlation_id",
        ] {
            captured.assert_absent(forbidden);
        }
    }
}

// Regression check for the OpenTelemetry 0.32 queue-occupancy invariant documented in `queue`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_exporters_preserve_exact_bounds_and_export_otlp_protobuf() {
    let _otlp_guard = otlp_test_lock().lock_owned().await;
    let (release_exports, _) = watch::channel(false);
    let state = Arc::new(ReceiverState {
        captures: Mutex::new(Captures::default()),
        started_requests: AtomicUsize::new(0),
        request_started: Notify::new(),
        release_exports,
    });
    let app = Router::new()
        .route("/v1/traces", post(receive_traces))
        .route("/v1/logs", post(receive_logs))
        .with_state(Arc::clone(&state));
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test receiver binds");
    let address = listener
        .local_addr()
        .expect("receiver address is available");
    let receiver = tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("test receiver serves requests");
    });
    let config = telemetry_config(&format!("http://{address}"));
    let (runtime, subscriber) = build_runtime("github_webhook_exporter=info", &config, io::sink)
        .expect("telemetry runtime initializes");
    let dispatch = Dispatch::new(subscriber);

    emit_records(&dispatch, 0..1);
    wait_for_blocked_signals(&state).await;
    emit_records(&dispatch, 1..SATURATION_RECORDS + 1);

    let expected_drops = (SATURATION_RECORDS - QUEUE_CAPACITY) as u64;
    assert_eq!(runtime.state(), TelemetryState::Enabled);
    assert_eq!(runtime.pending_trace_records(), QUEUE_CAPACITY);
    assert_eq!(runtime.pending_log_records(), QUEUE_CAPACITY);
    assert_eq!(runtime.dropped_trace_records(), expected_drops);
    assert_eq!(runtime.dropped_log_records(), expected_drops);
    state
        .release_exports
        .send(true)
        .expect("blocked exports are released");
    runtime.force_flush().expect("providers flush");

    assert_eq!(runtime.pending_trace_records(), 0);
    assert_eq!(runtime.pending_log_records(), 0);
    assert_eq!(runtime.failed_trace_exports(), 0);
    assert_eq!(runtime.failed_log_exports(), 0);
    let captures = state.captures.lock().expect("capture lock is available");
    let trace_batches = trace_batch_sizes(&captures.traces);
    let log_batches = log_batch_sizes(&captures.logs);
    assert_eq!(trace_batches.iter().sum::<usize>(), QUEUE_CAPACITY + 1);
    assert_eq!(log_batches.iter().sum::<usize>(), QUEUE_CAPACITY + 1);
    assert!(trace_batches.iter().all(|size| *size == 1));
    assert!(log_batches.iter().all(|size| *size == 1));
    let keys = resource_keys(
        captures
            .traces
            .first()
            .expect("at least one trace request is captured"),
    );
    assert!(keys.contains(&"service.name"));
    assert!(keys.contains(&"service.version"));
    assert!(keys.contains(&"k8s.pod.name"));
    assert!(keys.contains(&"k8s.namespace.name"));
    assert!(!keys.contains(&"forbidden"));

    receiver.abort();
}
