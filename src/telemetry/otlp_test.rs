use std::{
    collections::HashMap,
    ffi::OsString,
    io::{self, Write},
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
    security::{
        AdminAuthenticator, AdminToken, CanonicalRepositoryName, MasterKey, RepositorySecret,
        RepositorySecretCipher,
    },
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
const WEBHOOK_SECRET: &str = "webhook-trace-secret-must-not-appear";
const WEBHOOK_REPOSITORY: &str = "owner/webhook-private-repository";
const WEBHOOK_SHA_40: &str = "0123456789ABCDEF0123456789abcdef01234567";
const WEBHOOK_SHA_64: &str = "ABCDEF0123456789abcdef0123456789ABCDEF0123456789abcdef0123456789";

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
            .await
            .expect("test repository is created");
        let admin_token = AdminToken::new(ADMIN_TOKEN.to_owned()).expect("test token is valid");
        let router = build_router(AppState::new(
            store,
            AdminAuthenticator::new(&admin_token),
            2_097_152,
        ));
        Self {
            receiver,
            runtime,
            dispatch,
            output,
            router,
            pool,
            _directory: directory,
        }
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

    fn force_flush(&self) -> CapturedSpans {
        self.runtime.force_flush().expect("providers flush");
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
            .expect("matching HTTP request span is exported")
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

    fn descendant_named(&self, parent: &Span, name: &str, entity: &str) -> &Span {
        self.spans
            .iter()
            .find(|span| {
                span.name == name
                    && (string_attribute(span, "ghe.queue.entity") == Some(entity)
                        || string_attribute(span, "db.operation.name") == Some(entity))
                    && self.is_descendant(span, parent)
            })
            .expect("matching descendant span is exported")
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_successes_emit_bounded_hierarchy_and_span_only_identifiers() {
    let fixture = WebhookTraceFixture::new().await;
    let merge_group_delivery = "550e8400-e29b-41d4-a716-446655440201";
    let enqueue_delivery = "550e8400-e29b-41d4-a716-446655440202";
    let dequeue_delivery = "550e8400-e29b-41d4-a716-446655440203";
    let malformed_sha_delivery = "550e8400-e29b-41d4-a716-446655440204";
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

    let output = fixture.output.text();
    let normalized_sha_40 = WEBHOOK_SHA_40.to_ascii_lowercase();
    let normalized_sha_64 = WEBHOOK_SHA_64.to_ascii_lowercase();
    for identifier in [
        WEBHOOK_REPOSITORY,
        merge_group_delivery,
        enqueue_delivery,
        dequeue_delivery,
        malformed_sha_delivery,
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
    let malformed = fixture
        .webhook(
            malformed_body.as_bytes(),
            "pull_request",
            malformed_delivery,
            WEBHOOK_SECRET,
        )
        .await;
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
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
