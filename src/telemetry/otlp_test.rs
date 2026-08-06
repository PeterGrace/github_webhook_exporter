use std::{
    collections::HashMap,
    ffi::OsString,
    io,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
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
use opentelemetry_proto::tonic::{
    collector::{logs::v1::ExportLogsServiceRequest, trace::v1::ExportTraceServiceRequest},
    common::v1::any_value::Value as AttributeValue,
    trace::v1::Span,
};
use prost::Message;
use serde_json::Value;
use sqlx::SqlitePool;
use tempfile::TempDir;
use tower::ServiceExt;
use tracing::instrument::WithSubscriber;

use crate::{
    app::{build_router, AppState},
    security::{AdminAuthenticator, AdminToken, MasterKey, RepositorySecretCipher},
    storage::{open_database, RepositoryStore},
};
use tokio::{
    net::TcpListener,
    sync::{watch, Notify},
};
use tracing::Dispatch;

use crate::config::TelemetryConfig;

use super::{build_runtime, TelemetryState};

const QUEUE_CAPACITY: usize = 4;
const SATURATION_RECORDS: usize = 10;
const ADMIN_TOKEN: &str = "independent-admin-token";
const MASTER_KEY_BYTES: &[u8; 32] = b"MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM";

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
    let values = HashMap::from([
        ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint),
        ("OTEL_EXPORTER_OTLP_HEADERS", "x-test-token=private-value"),
        ("OTEL_EXPORTER_OTLP_TIMEOUT", "2000"),
        ("OTEL_SERVICE_NAME", "github-webhook-exporter-test"),
        (
            "OTEL_RESOURCE_ATTRIBUTES",
            "k8s.pod.name=exporter-0,k8s.namespace.name=tests,forbidden=value",
        ),
        ("GHE_OTEL_QUEUE_CAPACITY", "4"),
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

    fn trace_requests(&self) -> Vec<ExportTraceServiceRequest> {
        self.state
            .captures
            .lock()
            .expect("capture lock is available")
            .traces
            .clone()
    }
}

impl Drop for RunningReceiver {
    fn drop(&mut self) {
        self.task.abort();
    }
}

struct RepositoryTraceFixture {
    receiver: RunningReceiver,
    runtime: super::TelemetryRuntime,
    dispatch: Dispatch,
    router: Router,
    pool: SqlitePool,
    _directory: TempDir,
}

impl RepositoryTraceFixture {
    async fn new() -> Self {
        let receiver = RunningReceiver::start_released().await;
        let config = telemetry_config(&receiver.endpoint());
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
        self.runtime.force_flush().expect("providers flush");
        CapturedSpans::from_requests(self.receiver.trace_requests())
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
    serialized: Vec<u8>,
}

impl CapturedSpans {
    fn from_requests(requests: Vec<ExportTraceServiceRequest>) -> Self {
        let spans = requests
            .iter()
            .flat_map(|request| &request.resource_spans)
            .flat_map(|resource| &resource.scope_spans)
            .flat_map(|scope| scope.spans.iter().cloned())
            .collect();
        let serialized = requests
            .into_iter()
            .flat_map(|request| request.encode_to_vec())
            .collect();
        Self { spans, serialized }
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
            .expect("matching HTTP request span is exported")
    }

    fn child_named(&self, parent: &Span, name: &str) -> &Span {
        self.spans
            .iter()
            .find(|span| span.name == name && span.parent_span_id == parent.span_id)
            .expect("matching child span is exported")
    }

    fn child_count(&self, parent: &Span, name: &str) -> usize {
        self.spans
            .iter()
            .filter(|span| span.name == name && span.parent_span_id == parent.span_id)
            .count()
    }

    fn assert_absent(&self, value: &str) {
        let serialized = String::from_utf8_lossy(&self.serialized);
        assert!(
            !serialized.contains(value),
            "serialized OTLP trace requests must not contain {value:?}"
        );
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

    let captured = fixture.force_flush();
    let request = captured.http_request("POST", "/api/v1/repositories", 500);
    assert_attribute(request, "ghe.http.result", "server_error");
    let write = captured.child_named(request, "config.repository.write");
    assert_attribute(write, "ghe.config.operation", "create");
    assert_attribute(write, "github.repository.name", "owner/failure");
    assert_attribute(write, "ghe.operation.outcome", "failure");
    captured.assert_absent("failure-secret-must-not-appear");
    captured.assert_absent("database");
    captured.assert_absent("sqlite");
}

// Regression check for the OpenTelemetry 0.32 queue-occupancy invariant documented in `queue`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_exporters_preserve_exact_bounds_and_export_otlp_protobuf() {
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
