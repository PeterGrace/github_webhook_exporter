use std::{
    collections::{BTreeSet, HashMap},
    ffi::OsString,
    io::{self, Write},
    sync::{
        atomic::{AtomicU16, AtomicUsize, Ordering},
        Arc, Condvar, Mutex, OnceLock,
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
    common::v1::{
        any_value::Value as AttributeValue, AnyValue, ArrayValue, KeyValue, KeyValueList,
    },
    logs::v1::LogRecord,
    trace::v1::{span::SpanKind as OtlpSpanKind, status::StatusCode as OtlpStatusCode, Span},
};
use prost::Message;
use sentry::{
    protocol::{Context as SentryContext, SpanStatus as SentrySpanStatus},
    test::TestTransport,
    Level as SentryLevel, TransportFactory,
};
use serde_json::Value;
use sha2::Sha256;
use sqlx::SqlitePool;
use tempfile::TempDir;
use tower::ServiceExt;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::layer::{Context as SubscriberContext, Layer, SubscriberExt};

use crate::{
    app::{build_router, AppState},
    config::DEFAULT_WORKFLOW_JOB_MAX_STEPS,
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
use tracing::{
    span::{Attributes, Id},
    Dispatch, Instrument, Subscriber,
};

use crate::{config::TelemetryConfig, metrics::Metrics};
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

use super::{
    build_runtime, build_runtime_with_sentry_transport, trace, TelemetryShutdownOutcome,
    TelemetryState, LOCAL_ONLY_LOG_TARGET,
};

const QUEUE_CAPACITY: usize = 4;
const SATURATION_RECORDS: usize = 10;
const ADMIN_TOKEN: &str = "independent-admin-token";
const MASTER_KEY_BYTES: &[u8; 32] = b"MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM";
const WEBHOOK_SECRET: &str = "webhook-trace-secret-must-not-appear";
const WEBHOOK_REPOSITORY: &str = "owner/webhook-private-repository";
const ACTIONABLE_REPOSITORY: &str = "owner/repository";
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
const RAW_QUEUE_STORE_DETAIL: &str = "forbidden-queue-store-detail";
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
const PRIVACY_QUEUE_FAILURE_DELIVERY: &str = "71000000-0000-4000-8000-000000000007";
const PRIVACY_RETENTION_PR_NUMBER: i64 = PRIVACY_PR_NUMBER + 1;
const PRIVACY_QUEUE_FAILURE_PR_NUMBER: i64 = PRIVACY_PR_NUMBER + 2;
const WORKFLOW_PRIVACY_DELIVERY: &str = "71000000-0000-4000-8000-000000000008";
const WORKFLOW_PRIVACY_RUN_ID: i64 = 8_123_456_789_013;
const WORKFLOW_PRIVACY_RUN_ATTEMPT: i64 = 8_123_456_789_014;
const WORKFLOW_PRIVACY_JOB_ID: i64 = 8_123_456_789_015;
const WORKFLOW_PRIVACY_PR_NUMBERS: &[i64] = &[812_345_671, 812_345_672];
const WORKFLOW_PRIVACY_STEP_NUMBER: i64 = 73;
const WORKFLOW_PRIVACY_SHA: &str = "fedcba9876543210fedcba9876543210fedcba98";
const WORKFLOW_RAW_NAME: &str = "task6-workflow\nname-sentinel";
const WORKFLOW_ESCAPED_RAW_NAME: &str = "task6-workflow\\nname-sentinel";
const WORKFLOW_SANITIZED_NAME: &str = "task6-workflowname-sentinel";
const WORKFLOW_RAW_JOB_NAME: &str = "task6-job\tname-sentinel";
const WORKFLOW_ESCAPED_RAW_JOB_NAME: &str = "task6-job\\tname-sentinel";
const WORKFLOW_SANITIZED_JOB_NAME: &str = "task6-jobname-sentinel";
const WORKFLOW_RAW_STEP_NAME: &str = "task6-step\rname-sentinel";
const WORKFLOW_ESCAPED_RAW_STEP_NAME: &str = "task6-step\\rname-sentinel";
const WORKFLOW_SANITIZED_STEP_NAME: &str = "task6-stepname-sentinel";
const WORKFLOW_FORBIDDEN_COMMAND: &str = "task6-forbidden-command-sentinel";
const WORKFLOW_FORBIDDEN_OUTPUT: &str = "task6-forbidden-output-sentinel";
const WORKFLOW_FORBIDDEN_LOG: &str = "task6-forbidden-log-sentinel";
const WORKFLOW_FORBIDDEN_ACTOR: &str = "task6-forbidden-actor-sentinel";
const WORKFLOW_FORBIDDEN_URL: &str = "https://task6-forbidden-url.invalid/private";
const WORKFLOW_FORBIDDEN_SECRET: &str = "task6-forbidden-secret-sentinel";
const WORKFLOW_FORBIDDEN_SIGNATURE: &str =
    "sha256=6666666666666666666666666666666666666666666666666666666666666666";
const WORKFLOW_FORBIDDEN_HEADER: &str = "x-task6-private=forbidden-header-sentinel";
const WORKFLOW_FORBIDDEN_FRAGMENT: &str = "task6-forbidden-raw-payload-fragment-sentinel";
const WORKFLOW_UNKNOWN_CONCLUSION: &str = "task6-forbidden-unknown-conclusion-sentinel";
const WEBHOOK_DEBUG_SENTINEL: &str = "webhook-debug-sentinel";

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
    "cicd.pipeline.name",
    "cicd.pipeline.run.id",
    "cicd.pipeline.run.url.full",
    "cicd.pipeline.task.name",
    "cicd.pipeline.task.run.id",
    "cicd.pipeline.task.run.result",
    "cicd.pipeline.task.run.url.full",
    "error.type",
    "sentry.description",
    "sentry.op",
    "vcs.ref.head.revision",
    "vcs.repository.name",
    "vcs.repository.url.full",
    "github.workflow.conclusion",
    "github.workflow.run.attempt",
    "github.workflow.event",
    "github.workflow.source_branch",
    "github.workflow.target_branch",
    "timing_source",
    "db.system.name",
    "db.operation.name",
];
const SPAN_EVENT_ALLOWLIST: &[(&str, &[&str])] = &[
    ("operation.failure", &["ghe.failure.reason"]),
    ("exception", &["exception.type", "exception.message"]),
];
const SPAN_ONLY_ATTRIBUTE_KEYS: &[&str] = &[
    "cicd.pipeline.name",
    "cicd.pipeline.run.id",
    "cicd.pipeline.run.url.full",
    "cicd.pipeline.task.name",
    "cicd.pipeline.task.run.id",
    "cicd.pipeline.task.run.url.full",
    "sentry.description",
    "vcs.ref.head.revision",
    "vcs.repository.name",
    "vcs.repository.url.full",
    "github.repository.name",
    "github.repository.id",
    "github.delivery.id",
    "github.pull_request.number",
    "github.commit.sha",
    "github.workflow.run.attempt",
];

#[derive(Default)]
struct Captures {
    traces: Vec<ExportTraceServiceRequest>,
    logs: Vec<ExportLogsServiceRequest>,
}

struct ReceiverState {
    captures: Mutex<Captures>,
    response_status: AtomicU16,
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
    collector_response(&state)
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
    collector_response(&state)
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

fn collector_response(state: &ReceiverState) -> impl IntoResponse {
    let status = StatusCode::from_u16(state.response_status.load(Ordering::Acquire))
        .expect("configured collector status is valid");
    let body = if status.is_success() {
        Bytes::new()
    } else {
        Bytes::from_static(b"collector-secret-response-body")
    };
    (status, [(CONTENT_TYPE, "application/x-protobuf")], body)
}

fn telemetry_config(endpoint: &str) -> TelemetryConfig {
    telemetry_config_with_queue_capacity(endpoint, QUEUE_CAPACITY)
}

fn telemetry_config_with_queue_capacity(endpoint: &str, queue_capacity: usize) -> TelemetryConfig {
    telemetry_config_with_queue_capacity_and_timeout(endpoint, queue_capacity, 2_000)
}

fn telemetry_config_with_queue_capacity_and_timeout(
    endpoint: &str,
    queue_capacity: usize,
    timeout_millis: u64,
) -> TelemetryConfig {
    let queue_capacity = queue_capacity.to_string();
    let timeout_millis = timeout_millis.to_string();
    let values = HashMap::from([
        ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint),
        ("OTEL_EXPORTER_OTLP_HEADERS", "x-test-token=private-value"),
        ("OTEL_EXPORTER_OTLP_TIMEOUT", timeout_millis.as_str()),
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

fn telemetry_config_with_sentry(endpoint: &str, timeout_millis: u64) -> TelemetryConfig {
    let timeout_millis = timeout_millis.to_string();
    let values = HashMap::from([
        ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint),
        ("OTEL_EXPORTER_OTLP_HEADERS", "x-test-token=private-value"),
        ("OTEL_EXPORTER_OTLP_TIMEOUT", timeout_millis.as_str()),
        ("OTEL_SERVICE_NAME", "github-webhook-exporter-test"),
        ("GHE_OTEL_QUEUE_CAPACITY", "128"),
        ("GHE_OTEL_BATCH_SIZE", "1"),
        ("SENTRY_DSN", "https://public@sentry.invalid/1"),
    ]);
    TelemetryConfig::from_lookup(&mut |variable| values.get(variable).map(OsString::from))
        .expect("Sentry telemetry configuration is valid")
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

async fn wait_for_started_requests(state: &ReceiverState, target: usize) {
    while state.started_requests.load(Ordering::Acquire) < target {
        state.request_started.notified().await;
    }
}

struct RunningReceiver {
    state: SharedReceiverState,
    address: std::net::SocketAddr,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl RunningReceiver {
    async fn start_released() -> Self {
        let (release_exports, _) = watch::channel(true);
        let state = Arc::new(ReceiverState {
            captures: Mutex::new(Captures::default()),
            response_status: AtomicU16::new(StatusCode::OK.as_u16()),
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
            task: Some(task),
        }
    }

    async fn stop(&mut self) {
        let Some(task) = self.task.take() else {
            return;
        };
        task.abort();
        match task.await {
            Ok(()) => {}
            Err(error) if error.is_cancelled() => {}
            Err(error) => panic!("test receiver shutdown failed: {error}"),
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

    fn set_exports_released(&self, released: bool) {
        self.state.release_exports.send_replace(released);
    }

    fn set_response_status(&self, status: StatusCode) {
        self.state
            .response_status
            .store(status.as_u16(), Ordering::Release);
    }

    fn started_request_count(&self) -> usize {
        self.state.started_requests.load(Ordering::Acquire)
    }
}

impl Drop for RunningReceiver {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct RepositoryTraceFixture {
    _otlp_guard: OtlpTestGuard,
    receiver: RunningReceiver,
    runtime: super::TelemetryRuntime,
    dispatch: Dispatch,
    output: CapturedOutput,
    span_lifecycles: CapturedSpanLifecycles,
    router: Router,
    pool: SqlitePool,
    _directory: TempDir,
}

impl RepositoryTraceFixture {
    async fn new() -> Self {
        let otlp_guard = otlp_test_lock().lock_owned().await;
        let receiver = RunningReceiver::start_released().await;
        let config = telemetry_config_with_queue_capacity(&receiver.endpoint(), 128);
        let output = CapturedOutput::default();
        let (runtime, subscriber) = build_runtime(
            "github_webhook_exporter=info",
            &config,
            output.clone(),
            Metrics::new(),
        )
        .expect("telemetry runtime initializes");
        let span_lifecycles = CapturedSpanLifecycles::default();
        let dispatch = Dispatch::new(subscriber.with(span_lifecycles.clone()));
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
            output,
            span_lifecycles,
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

    fn finish(self) -> CapturedSpans {
        let Self {
            _otlp_guard,
            receiver,
            runtime,
            dispatch,
            output,
            span_lifecycles,
            router,
            pool,
            _directory,
        } = self;
        drop(router);
        drop(pool);
        drop(dispatch);
        drop(output);
        flush_after_span_closure(&runtime, &span_lifecycles);
        let (traces, logs) = receiver.captured_requests();
        drop(runtime);
        drop(receiver);
        drop(_directory);
        drop(_otlp_guard);
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SpanLifecycleEvent {
    Created,
    Closed,
}

#[derive(Default)]
struct SpanLifecycleState {
    span_names: HashMap<Id, &'static str>,
    events: Vec<(&'static str, SpanLifecycleEvent)>,
}

#[derive(Clone, Default)]
struct CapturedSpanLifecycles(Arc<(Mutex<SpanLifecycleState>, Condvar)>);

impl CapturedSpanLifecycles {
    fn wait_until_all_closed(&self) -> bool {
        let (state, closed) = self.0.as_ref();
        let state = state.lock().expect("span lifecycle lock is available");
        if state.span_names.is_empty() {
            return true;
        }
        let (state, _) = closed
            .wait_timeout_while(state, std::time::Duration::from_millis(10), |state| {
                !state.span_names.is_empty()
            })
            .expect("span lifecycle wait is available");
        state.span_names.is_empty()
    }

    fn assert_closed_before_created(&self, closed_name: &str, created_name: &str) {
        let (state, _) = self.0.as_ref();
        let state = state.lock().expect("span lifecycle lock is available");
        let closed_index = state
            .events
            .iter()
            .position(|event| *event == (closed_name, SpanLifecycleEvent::Closed))
            .unwrap_or_else(|| panic!("{closed_name} span close is captured"));
        let created_index = state
            .events
            .iter()
            .position(|event| *event == (created_name, SpanLifecycleEvent::Created))
            .unwrap_or_else(|| panic!("{created_name} span creation is captured"));
        assert!(
            closed_index < created_index,
            "{closed_name} must close before {created_name} is created"
        );
    }
}

impl<S> Layer<S> for CapturedSpanLifecycles
where
    S: Subscriber,
{
    fn on_new_span(
        &self,
        attributes: &Attributes<'_>,
        id: &Id,
        _context: SubscriberContext<'_, S>,
    ) {
        let name = attributes.metadata().name();
        let (state, _) = self.0.as_ref();
        let mut state = state.lock().expect("span lifecycle lock is available");
        state.span_names.insert(id.clone(), name);
        state.events.push((name, SpanLifecycleEvent::Created));
    }

    fn on_close(&self, id: Id, _context: SubscriberContext<'_, S>) {
        let (state, closed) = self.0.as_ref();
        let mut state = state.lock().expect("span lifecycle lock is available");
        if let Some(name) = state.span_names.remove(&id) {
            state.events.push((name, SpanLifecycleEvent::Closed));
        }
        closed.notify_all();
    }
}

fn flush_after_span_closure(
    runtime: &super::TelemetryRuntime,
    span_lifecycles: &CapturedSpanLifecycles,
) {
    const MAX_PASSES: usize = 8;
    for _ in 0..MAX_PASSES {
        tokio::task::block_in_place(|| runtime.force_flush().expect("providers flush"));
        if span_lifecycles.wait_until_all_closed() {
            tokio::task::block_in_place(|| runtime.force_flush().expect("providers flush"));
            return;
        }
    }
    tokio::task::block_in_place(|| runtime.force_flush().expect("providers flush"));
}

struct WebhookTraceFixture {
    _otlp_guard: OtlpTestGuard,
    receiver: RunningReceiver,
    runtime: super::TelemetryRuntime,
    dispatch: Dispatch,
    output: CapturedOutput,
    span_lifecycles: CapturedSpanLifecycles,
    router: Router,
    pool: SqlitePool,
    _directory: TempDir,
}

impl WebhookTraceFixture {
    async fn new() -> Self {
        Self::new_with_exporter_timeout_and_step_limit(
            2_000,
            DEFAULT_WORKFLOW_JOB_MAX_STEPS,
            false,
            "github_webhook_exporter=info",
            None,
        )
        .await
    }

    async fn new_with_sentry() -> (Self, Arc<TestTransport>) {
        let transport = TestTransport::new();
        let factory: Arc<dyn TransportFactory> = Arc::new(transport.clone());
        let fixture = Self::new_with_exporter_timeout_and_step_limit(
            2_000,
            DEFAULT_WORKFLOW_JOB_MAX_STEPS,
            false,
            "github_webhook_exporter=info",
            Some(factory),
        )
        .await;
        (fixture, transport)
    }

    async fn new_with_debug_logging() -> Self {
        Self::new_with_exporter_timeout_and_step_limit(
            2_000,
            DEFAULT_WORKFLOW_JOB_MAX_STEPS,
            false,
            "github_webhook_exporter=debug",
            None,
        )
        .await
    }

    async fn new_with_workflow_job_max_steps(workflow_job_max_steps: usize) -> Self {
        Self::new_with_exporter_timeout_and_step_limit(
            2_000,
            workflow_job_max_steps,
            true,
            "github_webhook_exporter=info",
            None,
        )
        .await
    }

    async fn new_with_exporter_timeout(timeout_millis: u64) -> Self {
        Self::new_with_exporter_timeout_and_step_limit(
            timeout_millis,
            DEFAULT_WORKFLOW_JOB_MAX_STEPS,
            false,
            "github_webhook_exporter=info",
            None,
        )
        .await
    }

    async fn new_with_exporter_timeout_and_step_limit(
        timeout_millis: u64,
        workflow_job_max_steps: usize,
        include_actionable_repository: bool,
        rust_log: &str,
        sentry_transport: Option<Arc<dyn TransportFactory>>,
    ) -> Self {
        let otlp_guard = otlp_test_lock().lock_owned().await;
        let receiver = RunningReceiver::start_released().await;
        let config = if sentry_transport.is_some() {
            telemetry_config_with_sentry(&receiver.endpoint(), timeout_millis)
        } else {
            telemetry_config_with_queue_capacity_and_timeout(
                &receiver.endpoint(),
                128,
                timeout_millis,
            )
        };
        let output = CapturedOutput::default();
        let metrics = Metrics::new();
        let (runtime, subscriber) = build_runtime_with_sentry_transport(
            rust_log,
            &config,
            output.clone(),
            metrics.clone(),
            sentry_transport,
        )
        .expect("telemetry runtime initializes");
        let span_lifecycles = CapturedSpanLifecycles::default();
        let dispatch = Dispatch::new(subscriber.with(span_lifecycles.clone()));
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
        if include_actionable_repository {
            store
                .create(
                    CanonicalRepositoryName::new(ACTIONABLE_REPOSITORY)
                        .expect("test repository name is valid"),
                    RepositorySecret::new(WEBHOOK_SECRET.to_owned())
                        .expect("test webhook secret is valid"),
                    true,
                )
                .with_subscriber(dispatch.clone())
                .await
                .expect("test repository is created");
        }
        tokio::task::block_in_place(|| runtime.force_flush().expect("setup telemetry flushes"));
        receiver.clear_captured_requests();
        let admin_token = AdminToken::new(ADMIN_TOKEN.to_owned()).expect("test token is valid");
        let router = build_router(
            AppState::new(
                store,
                AdminAuthenticator::new(&admin_token),
                2_097_152,
                workflow_job_max_steps,
            )
            .with_metrics(metrics)
            .with_workflow_trace_emitter(runtime.workflow_trace_emitter()),
        );
        Self {
            _otlp_guard: otlp_guard,
            receiver,
            runtime,
            dispatch,
            output,
            span_lifecycles,
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
        self.webhook_with_authorization(body, event_type, delivery_id, secret, None)
            .await
    }

    async fn webhook_with_authorization(
        &self,
        body: &[u8],
        event_type: &str,
        delivery_id: &str,
        secret: &str,
        authorization: Option<&str>,
    ) -> axum::response::Response {
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC key is valid");
        mac.update(body);
        let signature = format!("sha256={}", hex::encode(mac.finalize().into_bytes()));
        let mut request = Request::builder()
            .method(Method::POST)
            .uri("/webhooks/github")
            .header(CONTENT_TYPE, "application/json")
            .header("X-GitHub-Event", event_type)
            .header("X-GitHub-Delivery", delivery_id)
            .header("X-Hub-Signature-256", signature);
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }
        let request = request
            .body(Body::from(body.to_vec()))
            .expect("webhook request is valid");
        if let Some(expected) = authorization {
            assert_eq!(
                request
                    .headers()
                    .get(header::AUTHORIZATION)
                    .and_then(|value| value.to_str().ok()),
                Some(expected),
                "webhook Authorization header contains the sentinel before submission"
            );
        }
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
        flush_after_span_closure(&self.runtime, &self.span_lifecycles);
    }

    fn force_flush(&self) -> CapturedSpans {
        let dropped_traces = self.runtime.dropped_trace_records();
        self.flush();
        assert_eq!(dropped_traces, 0, "test trace queue must not drop records");
        let (traces, logs) = self.receiver.captured_requests();
        CapturedSpans::from_requests(traces, logs)
    }

    async fn finish_without_force_flush(self) -> String {
        let Self {
            _otlp_guard,
            mut receiver,
            runtime,
            dispatch,
            output,
            span_lifecycles: _,
            router,
            pool,
            _directory,
        } = self;
        drop(router);
        drop(pool);
        drop(dispatch);
        drop(_directory);
        drop(runtime);
        receiver.stop().await;
        drop(receiver);
        let stderr = output.text();
        drop(_otlp_guard);
        stderr
    }

    fn shutdown(self) -> (CapturedSpans, String, TelemetryShutdownOutcome) {
        let Self {
            _otlp_guard,
            receiver,
            mut runtime,
            dispatch,
            output,
            span_lifecycles: _,
            router,
            pool,
            _directory,
        } = self;
        drop(router);
        drop(pool);
        drop(dispatch);
        drop(_directory);
        let outcome = runtime.shutdown(std::time::Duration::from_secs(2));
        assert_eq!(
            runtime.pending_trace_records(),
            0,
            "completed shutdown releases every accepted trace slot"
        );
        assert_eq!(
            runtime.pending_log_records(),
            0,
            "completed shutdown releases every accepted log slot"
        );
        let (traces, logs) = receiver.captured_requests();
        let captured = CapturedSpans::from_requests(traces, logs);
        let stderr = output.text();
        drop(runtime);
        drop(receiver);
        drop(_otlp_guard);
        (captured, stderr, outcome)
    }

    fn finish(self) -> (CapturedSpans, String) {
        // Destructuring permits the request-serving resources to drop before the providers flush,
        // so completed failure-path spans cannot remain owned by the in-process router or pool.
        let Self {
            _otlp_guard,
            receiver,
            runtime,
            dispatch,
            output,
            span_lifecycles,
            router,
            pool,
            _directory,
        } = self;
        drop(router);
        drop(pool);
        drop(dispatch);
        drop(_directory);
        flush_after_span_closure(&runtime, &span_lifecycles);
        assert_eq!(
            runtime.dropped_trace_records(),
            0,
            "test trace queue must not drop records"
        );
        let (traces, logs) = receiver.captured_requests();
        let captured = CapturedSpans::from_requests(traces, logs);
        let stderr = output.text();
        drop(runtime);
        drop(receiver);
        drop(_otlp_guard);
        (captured, stderr)
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
        DEFAULT_WORKFLOW_JOB_MAX_STEPS,
    ))
}

#[derive(Debug)]
struct CapturedSpans {
    spans: Vec<Span>,
    log_records: Vec<LogRecord>,
    log_resource_attributes: Vec<KeyValue>,
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
        let log_records = log_requests
            .iter()
            .flat_map(|request| &request.resource_logs)
            .flat_map(|resource| &resource.scope_logs)
            .flat_map(|scope| scope.log_records.iter().cloned())
            .collect();
        let log_resource_attributes = log_requests
            .iter()
            .flat_map(|request| &request.resource_logs)
            .filter_map(|resource_logs| resource_logs.resource.as_ref())
            .flat_map(|resource| resource.attributes.iter().cloned())
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
            log_records,
            log_resource_attributes,
            resource_attribute_keys,
            serialized,
            serialized_logs,
        }
    }

    fn from_parsed_records(
        spans: Vec<Span>,
        log_records: Vec<LogRecord>,
        log_resource_attributes: Vec<KeyValue>,
        serialized: Vec<u8>,
        serialized_logs: Vec<u8>,
    ) -> Self {
        Self {
            spans,
            log_records,
            log_resource_attributes,
            resource_attribute_keys: BTreeSet::new(),
            serialized,
            serialized_logs,
        }
    }

    fn trace_attributes(&self) -> impl Iterator<Item = &KeyValue> {
        self.spans.iter().flat_map(|span| {
            span.attributes
                .iter()
                .chain(span.events.iter().flat_map(|event| event.attributes.iter()))
                .chain(span.links.iter().flat_map(|link| link.attributes.iter()))
        })
    }

    fn log_attributes(&self) -> impl Iterator<Item = &KeyValue> {
        self.log_records
            .iter()
            .flat_map(|record| record.attributes.iter())
            .chain(self.log_resource_attributes.iter())
    }

    fn has_trace_attribute_key(&self, key: &str) -> bool {
        self.trace_attributes()
            .any(|attribute| attribute.key == key)
    }

    fn has_trace_i64_attribute(&self, key: &str, value: i64) -> bool {
        self.trace_attributes()
            .any(|attribute| i64_key_value(attribute, key) == Some(value))
    }

    fn has_trace_i64_array_attribute(&self, key: &str, values: &[i64]) -> bool {
        self.trace_attributes()
            .any(|attribute| i64_array_key_value(attribute, key) == Some(values.to_vec()))
    }

    fn has_log_i64_attribute(&self, key: &str, value: i64) -> bool {
        self.log_attributes()
            .any(|attribute| i64_key_value(attribute, key) == Some(value))
    }

    fn has_log_string_attribute(&self, key: &str, value: &str) -> bool {
        self.log_attributes()
            .any(|attribute| string_key_value(attribute, key) == Some(value))
    }

    fn has_trace_i64_value(&self, value: i64) -> bool {
        self.trace_attributes()
            .any(|attribute| i64_key_value(attribute, &attribute.key) == Some(value))
    }

    fn has_log_i64_array_attribute(&self, key: &str, values: &[i64]) -> bool {
        self.log_attributes()
            .any(|attribute| i64_array_key_value(attribute, key) == Some(values.to_vec()))
    }

    fn has_log_i64_value(&self, value: i64) -> bool {
        self.log_attributes()
            .filter_map(|attribute| attribute.value.as_ref())
            .chain(
                self.log_records
                    .iter()
                    .filter_map(|record| record.body.as_ref()),
            )
            .any(|candidate| any_value_contains_i64(candidate, value))
    }

    fn has_trace_string_attribute(&self, key: &str, value: &str) -> bool {
        self.trace_attributes()
            .any(|attribute| string_key_value(attribute, key) == Some(value))
    }

    fn has_log_attribute_key(&self, key: &str) -> bool {
        self.log_attributes().any(|attribute| attribute.key == key)
    }

    fn has_log_body(&self, expected: &str) -> bool {
        self.log_records.iter().any(|record| {
            record
                .body
                .as_ref()
                .and_then(string_any_value)
                .is_some_and(|body| body.contains(expected))
        })
    }

    fn workflow_rejection_log(&self) -> &LogRecord {
        let matches = self
            .log_records
            .iter()
            .filter(|record| {
                record.body.as_ref().and_then(string_any_value)
                    == Some("completed workflow-job trace rejected")
            })
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1, "workflow rejection warning count");
        matches[0]
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

    fn workflow_job_for_delivery(&self, delivery_id: &str) -> &Span {
        let matches = self
            .spans
            .iter()
            .filter(|span| {
                string_attribute(span, "sentry.op") == Some("github.actions.job")
                    && string_attribute(span, "github.delivery.id") == Some(delivery_id)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            matches.len(),
            1,
            "workflow job span count for {delivery_id}"
        );
        matches[0]
    }

    fn workflow_step<'spans>(&'spans self, job: &'spans Span) -> &'spans Span {
        let matches = self
            .children(job)
            .filter(|span| string_attribute(span, "sentry.op") == Some("github.actions.step"))
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1, "workflow step count");
        matches[0]
    }

    fn workflow_step_for_task_run_id<'spans>(
        &'spans self,
        job: &'spans Span,
        task_run_id: &str,
    ) -> &'spans Span {
        let matches = self
            .children(job)
            .filter(|span| {
                string_attribute(span, "sentry.op") == Some("github.actions.step")
                    && string_attribute(span, "cicd.pipeline.task.run.id") == Some(task_run_id)
            })
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1, "workflow step count for {task_run_id}");
        matches[0]
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

    fn parent_named(&self, child: &Span, name: &str) -> &Span {
        self.spans
            .iter()
            .find(|span| span.name == name && span.span_id == child.parent_span_id)
            .expect("matching parent span is exported")
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

fn is_workflow_task_span(span: &Span) -> bool {
    matches!(
        string_attribute(span, "sentry.op"),
        Some("github.actions.job" | "github.actions.step")
    )
}

fn string_attribute<'span>(span: &'span Span, key: &str) -> Option<&'span str> {
    span.attributes
        .iter()
        .find_map(|attribute| string_key_value(attribute, key))
}

fn event_string_attribute<'event>(
    event: &'event opentelemetry_proto::tonic::trace::v1::span::Event,
    key: &str,
) -> Option<&'event str> {
    event
        .attributes
        .iter()
        .find_map(|attribute| string_key_value(attribute, key))
}

fn assert_exception_event(
    span: &Span,
    expected_type: &str,
    expected_message: &str,
    expected_timestamp: u64,
) {
    let exception_events = span
        .events
        .iter()
        .filter(|event| event.name == "exception")
        .collect::<Vec<_>>();
    assert_eq!(
        exception_events.len(),
        1,
        "exception event count for {}",
        span.name
    );
    let event = exception_events[0];
    assert_eq!(event.name, "exception");
    assert_eq!(event.time_unix_nano, expected_timestamp);
    assert_eq!(event.attributes.len(), 2);
    assert_eq!(
        event_string_attribute(event, "exception.type"),
        Some(expected_type)
    );
    assert_eq!(
        event_string_attribute(event, "exception.message"),
        Some(expected_message)
    );
}

fn assert_no_exception_events(span: &Span) {
    assert_eq!(
        span.events
            .iter()
            .filter(|event| event.name == "exception")
            .count(),
        0,
        "unexpected exception events for {}",
        span.name
    );
}

fn string_key_value<'attribute>(
    attribute: &'attribute KeyValue,
    key: &str,
) -> Option<&'attribute str> {
    if attribute.key != key {
        return None;
    }
    attribute.value.as_ref().and_then(string_any_value)
}

fn string_any_value(value: &AnyValue) -> Option<&str> {
    match value.value.as_ref() {
        Some(AttributeValue::StringValue(value)) => Some(value.as_str()),
        Some(AttributeValue::IntValue(_))
        | Some(AttributeValue::DoubleValue(_))
        | Some(AttributeValue::BoolValue(_))
        | Some(AttributeValue::ArrayValue(_))
        | Some(AttributeValue::KvlistValue(_))
        | Some(AttributeValue::BytesValue(_))
        | Some(AttributeValue::StringValueStrindex(_))
        | None => None,
    }
}

fn any_value_contains_i64(value: &AnyValue, expected: i64) -> bool {
    match value.value.as_ref() {
        Some(AttributeValue::StringValue(value)) => value.parse() == Ok(expected),
        Some(AttributeValue::IntValue(value)) => *value == expected,
        Some(AttributeValue::DoubleValue(value)) => {
            let expected_as_f64 = expected as f64;
            expected_as_f64 as i64 == expected && *value == expected_as_f64
        }
        Some(AttributeValue::ArrayValue(values)) => values
            .values
            .iter()
            .any(|value| any_value_contains_i64(value, expected)),
        Some(AttributeValue::KvlistValue(values)) => values.values.iter().any(|attribute| {
            attribute
                .value
                .as_ref()
                .is_some_and(|value| any_value_contains_i64(value, expected))
        }),
        Some(AttributeValue::BoolValue(_))
        | Some(AttributeValue::BytesValue(_))
        | Some(AttributeValue::StringValueStrindex(_))
        | None => false,
    }
}

fn i64_attribute(span: &Span, key: &str) -> Option<i64> {
    span.attributes
        .iter()
        .find_map(|attribute| i64_key_value(attribute, key))
}

fn i64_array_attribute(span: &Span, key: &str) -> Option<Vec<i64>> {
    span.attributes
        .iter()
        .find_map(|attribute| i64_array_key_value(attribute, key))
}

fn i64_array_key_value(attribute: &KeyValue, key: &str) -> Option<Vec<i64>> {
    if attribute.key != key {
        return None;
    }
    let values = attribute
        .value
        .as_ref()
        .and_then(|value| match value.value.as_ref() {
            Some(AttributeValue::ArrayValue(values)) => Some(&values.values),
            Some(AttributeValue::StringValue(_))
            | Some(AttributeValue::IntValue(_))
            | Some(AttributeValue::DoubleValue(_))
            | Some(AttributeValue::BoolValue(_))
            | Some(AttributeValue::KvlistValue(_))
            | Some(AttributeValue::BytesValue(_))
            | Some(AttributeValue::StringValueStrindex(_))
            | None => None,
        })?;
    values
        .iter()
        .map(|value| match value.value.as_ref() {
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
        .collect()
}

fn i64_key_value(attribute: &KeyValue, key: &str) -> Option<i64> {
    if attribute.key != key {
        return None;
    }
    attribute
        .value
        .as_ref()
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

fn assert_i64_array_attribute(span: &Span, key: &str, value: &[i64]) {
    assert_eq!(i64_array_attribute(span, key), Some(value.to_vec()));
}

fn rfc3339_unix_nanos(value: &str) -> u64 {
    OffsetDateTime::parse(value, &Rfc3339)
        .expect("timestamp is RFC3339")
        .unix_timestamp_nanos() as u64
}

fn workflow_job_body(action: Option<&str>, workflow_job: Value) -> Vec<u8> {
    workflow_job_body_for_repository(action, workflow_job, WEBHOOK_REPOSITORY)
}

fn workflow_job_body_for_repository(
    action: Option<&str>,
    workflow_job: Value,
    repository_name: &str,
) -> Vec<u8> {
    let mut payload = serde_json::Map::with_capacity(3);
    if let Some(action) = action {
        payload.insert("action".to_owned(), Value::String(action.to_owned()));
    }
    payload.insert("workflow_job".to_owned(), workflow_job);
    payload.insert(
        "repository".to_owned(),
        serde_json::json!({"full_name": repository_name}),
    );
    serde_json::to_vec(&Value::Object(payload)).expect("workflow-job payload serializes")
}

fn assert_otlp_status(span: &Span, code: OtlpStatusCode, description: &str) {
    let status = span.status.as_ref().expect("span status is exported");
    assert_eq!(status.code, code as i32, "status code for {}", span.name);
    assert_eq!(
        status.message, description,
        "status description for {}",
        span.name
    );
}

fn assert_historical_interval(span: &Span, start: u64, end: u64, source: &str) {
    assert_eq!(
        span.start_time_unix_nano, start,
        "start time for {}",
        span.name
    );
    assert_eq!(span.end_time_unix_nano, end, "end time for {}", span.name);
    assert_attribute(span, "timing_source", source);
    for timestamp in [start, end] {
        assert!(
            OffsetDateTime::from_unix_timestamp_nanos(i128::from(timestamp)).is_ok(),
            "{} timestamp is a valid Unix nanosecond value",
            span.name
        );
    }
}

fn current_unix_nanos() -> u64 {
    u64::try_from(OffsetDateTime::now_utc().unix_timestamp_nanos())
        .expect("current test time is after the Unix epoch")
}

fn assert_metric_line(exposition: &str, expected: &str) {
    assert!(
        exposition.lines().any(|line| line == expected),
        "missing metric line {expected:?}"
    );
}

fn metric_u64(exposition: &str, series: &str) -> u64 {
    let prefix = format!("{series} ");
    exposition
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("missing metric series {series:?}"))
        .parse()
        .unwrap_or_else(|_| panic!("metric series {series:?} is an integer"))
}

fn metric_u64_or_zero(exposition: &str, series: &str) -> u64 {
    let prefix = format!("{series} ");
    exposition
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .map(|value| {
            value
                .parse()
                .unwrap_or_else(|_| panic!("metric series {series:?} is an integer"))
        })
        .unwrap_or(0)
}

#[test]
fn parsed_numeric_privacy_checks_detect_varint_attributes() {
    const TRACE_KEY: &str = "github.repository.id";
    const LOG_KEY: &str = "github.pull_request.number";
    const TRACE_VALUE: i64 = 987_000_001;
    const LOG_VALUE: i64 = 976_543_211;
    const LOG_BODY_VALUE: i64 = 8_123_456_789_013;
    const LOG_RESOURCE_VALUE: i64 = 8_123_456_789_014;

    let trace_span = Span {
        attributes: vec![KeyValue {
            key: TRACE_KEY.to_owned(),
            value: Some(AnyValue {
                value: Some(AttributeValue::IntValue(TRACE_VALUE)),
            }),
            key_strindex: 0,
        }],
        ..Span::default()
    };
    let log_record = LogRecord {
        attributes: vec![KeyValue {
            key: LOG_KEY.to_owned(),
            value: Some(AnyValue {
                value: Some(AttributeValue::IntValue(LOG_VALUE)),
            }),
            key_strindex: 0,
        }],
        body: Some(AnyValue {
            value: Some(AttributeValue::ArrayValue(ArrayValue {
                values: vec![AnyValue {
                    value: Some(AttributeValue::KvlistValue(KeyValueList {
                        values: vec![KeyValue {
                            key: "nested".to_owned(),
                            value: Some(AnyValue {
                                value: Some(AttributeValue::IntValue(LOG_BODY_VALUE)),
                            }),
                            key_strindex: 0,
                        }],
                    })),
                }],
            })),
        }),
        ..LogRecord::default()
    };
    let log_resource_attribute = KeyValue {
        key: "resource.numeric".to_owned(),
        value: Some(AnyValue {
            value: Some(AttributeValue::IntValue(LOG_RESOURCE_VALUE)),
        }),
        key_strindex: 0,
    };
    let serialized_trace = trace_span.encode_to_vec();
    let serialized_log = log_record.encode_to_vec();
    assert!(!String::from_utf8_lossy(&serialized_trace).contains(&TRACE_VALUE.to_string()));
    assert!(!String::from_utf8_lossy(&serialized_log).contains(&LOG_VALUE.to_string()));

    let captured = CapturedSpans::from_parsed_records(
        vec![trace_span],
        vec![log_record],
        vec![log_resource_attribute],
        serialized_trace,
        serialized_log,
    );
    assert!(captured.has_trace_i64_attribute(TRACE_KEY, TRACE_VALUE));
    assert!(captured.has_log_i64_attribute(LOG_KEY, LOG_VALUE));
    assert!(captured.has_log_i64_value(LOG_BODY_VALUE));
    assert!(captured.has_log_i64_value(LOG_RESOURCE_VALUE));
    assert!(captured.has_log_attribute_key(LOG_KEY));
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
        span_lifecycles: CapturedSpanLifecycles,
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
            let (runtime, subscriber) = build_runtime(
                "github_webhook_exporter=info",
                &config,
                io::sink,
                Metrics::new(),
            )
            .expect("telemetry runtime initializes");
            let span_lifecycles = CapturedSpanLifecycles::default();
            let dispatch = Dispatch::new(subscriber.with(span_lifecycles.clone()));
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
                span_lifecycles,
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

        fn finish(self) -> (CapturedSpans, String) {
            let Self {
                _otlp_guard,
                receiver,
                runtime,
                dispatch,
                span_lifecycles,
                pool,
                database_path,
                repository_store,
                delivery_store,
                merge_queue_store,
                _directory,
            } = self;
            drop(repository_store);
            drop(delivery_store);
            drop(merge_queue_store);
            drop(pool);
            drop(dispatch);
            flush_after_span_closure(&runtime, &span_lifecycles);
            let (traces, logs) = receiver.captured_requests();
            drop(runtime);
            drop(receiver);
            drop(_directory);
            drop(_otlp_guard);
            (CapturedSpans::from_requests(traces, logs), database_path)
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

        let (captured, database_path) = fixture.finish();
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
        for forbidden in ["sqlite-secret-must-not-appear", database_path.as_str()] {
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

        let (captured, database_path) = fixture.finish();
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
            database_path.as_str(),
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

    let captured = fixture.finish();
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

    let captured = fixture.finish();
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

    let output = fixture.output.clone();
    let captured = fixture.finish();
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
    captured.assert_approved_attribute_keys();
    assert!(captured.has_log_body("request failed"));
    assert!(output.text().contains("request failed"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_completion_is_local_debug_only() {
    let fixture = WebhookTraceFixture::new_with_debug_logging().await;
    let delivery_id = "550e8400-e29b-41d4-a716-446655440200";
    let body = format!(r#"{{"repository":{{"full_name":"{WEBHOOK_REPOSITORY}"}}}}"#);

    let response = fixture
        .webhook(body.as_bytes(), "ping", delivery_id, WEBHOOK_SECRET)
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    drop(response);

    tracing::dispatcher::with_default(&fixture.dispatch, || {
        tracing::debug!(target: "github_webhook_exporter", "{WEBHOOK_DEBUG_SENTINEL}");
    });
    tokio::task::block_in_place(|| fixture.runtime.force_flush().expect("providers flush"));

    let stderr = fixture.output.text();
    let completion_line = stderr
        .lines()
        .find(|line| line.contains("GitHub webhook request processed"))
        .expect("local completion log is rendered to stderr");
    assert!(completion_line.contains("DEBUG"));
    assert!(completion_line.contains(LOCAL_ONLY_LOG_TARGET));

    let (captured, _) = fixture.finish();
    assert!(captured.has_log_body(WEBHOOK_DEBUG_SENTINEL));
    assert!(!captured.has_log_body("GitHub webhook request processed"));
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
    fixture
        .span_lifecycles
        .assert_closed_before_created("github.webhook.authenticate", "github.webhook.process");
    let exposition = fixture.metrics_text().await;
    let (captured, output) = fixture.finish();

    let merge_request = captured.webhook_request_for_delivery(merge_group_delivery);
    assert_attribute(merge_request, "github.repository.name", WEBHOOK_REPOSITORY);
    let merge_authenticate = captured.child_named(merge_request, "github.webhook.authenticate");
    let merge_process = captured.child_named(merge_request, "github.webhook.process");
    assert!(
        merge_authenticate.end_time_unix_nano <= merge_process.start_time_unix_nano,
        "authentication must end before webhook processing starts"
    );
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

    let normalized_sha_40 = WEBHOOK_SHA_40.to_ascii_lowercase();
    let normalized_sha_64 = WEBHOOK_SHA_64.to_ascii_lowercase();
    assert!(exposition.contains(&format!("repository=\"{WEBHOOK_REPOSITORY}\"")));
    captured.assert_logs_absent(WEBHOOK_REPOSITORY);
    assert!(!output.contains(WEBHOOK_REPOSITORY));
    for identifier in [
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
async fn shutdown_exports_accepted_core_workflow_and_log_records() {
    let fixture = WebhookTraceFixture::new().await;
    let delivery_id = "550e8400-e29b-41d4-a716-446655440299";
    let body = serde_json::to_vec(&serde_json::json!({
        "action": "completed",
        "workflow_job": {
            "id": 901,
            "run_id": 902,
            "run_attempt": 1,
            "conclusion": "success",
            "head_sha": WEBHOOK_SHA_40,
            "started_at": "2026-08-07T12:00:00Z",
            "completed_at": "2026-08-07T12:01:00Z",
            "steps": [{
                "number": 1,
                "name": "shutdown step",
                "conclusion": "success",
                "started_at": "2026-08-07T12:00:10Z",
                "completed_at": "2026-08-07T12:00:20Z"
            }]
        },
        "repository": {"full_name": WEBHOOK_REPOSITORY}
    }))
    .expect("workflow-job payload serializes");
    let response = fixture
        .webhook(&body, "workflow_job", delivery_id, WEBHOOK_SECRET)
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    drop(response);
    tracing::dispatcher::with_default(&fixture.dispatch, || {
        tracing::info!(target: "github_webhook_exporter", "shutdown export sentinel");
    });

    let (captured, stderr, outcome) = fixture.shutdown();

    assert_eq!(outcome, TelemetryShutdownOutcome::Completed);
    assert!(captured
        .webhook_request_for_delivery(delivery_id)
        .parent_span_id
        .is_empty());
    let job = captured.one_named("workflow / job");
    assert_eq!(captured.child_count(job, "shutdown step"), 1);
    assert!(captured.has_log_body("shutdown export sentinel"));
    assert!(stderr.contains("shutdown export sentinel"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_job_completed_exports_one_independent_historical_trace() {
    let fixture = WebhookTraceFixture::new().await;
    let delivery_id = "550e8400-e29b-41d4-a716-446655440207";
    let workflow_run_delivery_id = "550e8400-e29b-41d4-a716-446655440208";
    let job_started_at = "2026-08-06T10:00:00.123456789Z";
    let first_step_started_at = "2026-08-06T10:00:01.000000001Z";
    let first_step_completed_at = "2026-08-06T10:00:02.000000002Z";
    let second_step_started_at = "2026-08-06T10:00:03.000000003Z";
    let second_step_completed_at = "2026-08-06T10:00:04.000000004Z";
    let job_completed_at = "2026-08-06T10:05:00.987654321Z";
    let pull_requests = (1..=25)
        .map(|number| serde_json::json!({"number": number}))
        .collect::<Vec<_>>();
    let workflow_run_body = serde_json::to_vec(&serde_json::json!({
        "action": "requested",
        "workflow_run": {
            "id": 31,
            "run_attempt": 2,
            "event": "merge_group",
            "head_branch": "gh-readonly-queue/main/pr-7-deadbeef",
            "pull_requests": [{
                "head": {"ref": "feature/source"},
                "base": {"ref": "main"}
            }]
        },
        "repository": {"full_name": WEBHOOK_REPOSITORY}
    }))
    .expect("workflow-run payload serializes");
    let workflow_run_response = fixture
        .webhook(
            &workflow_run_body,
            "workflow_run",
            workflow_run_delivery_id,
            WEBHOOK_SECRET,
        )
        .await;
    assert_eq!(workflow_run_response.status(), StatusCode::NO_CONTENT);
    drop(workflow_run_response);

    let body = serde_json::to_vec(&serde_json::json!({
        "action": "completed",
        "workflow_job": {
            "id": 41,
            "run_id": 31,
            "run_attempt": 2,
            "workflow_name": "Build\nWorkflow",
            "name": "Linux\tJob",
            "conclusion": "success",
            "head_sha": WEBHOOK_SHA_40,
            "started_at": job_started_at,
            "completed_at": job_completed_at,
            "pull_requests": pull_requests,
            "steps": [
                {
                    "number": 2,
                    "name": "Run\tTests",
                    "conclusion": "success",
                    "started_at": first_step_started_at,
                    "completed_at": first_step_completed_at
                },
                {
                    "number": 1,
                    "name": "Check\nout",
                    "conclusion": "success",
                    "started_at": second_step_started_at,
                    "completed_at": second_step_completed_at
                }
            ]
        },
        "repository": {
            "full_name": WEBHOOK_REPOSITORY
        }
    }))
    .expect("workflow-job payload serializes");

    let response = fixture
        .webhook(&body, "workflow_job", delivery_id, WEBHOOK_SECRET)
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    drop(response);

    let captured = fixture.force_flush();
    let request = captured.webhook_request_for_delivery(delivery_id);
    let job = captured.one_named("BuildWorkflow / LinuxJob");
    assert!(job.parent_span_id.is_empty());
    assert_eq!(captured.children(job).count(), 2);
    assert_ne!(job.trace_id, request.trace_id);
    assert_eq!(job.kind, OtlpSpanKind::Internal as i32);
    assert_attribute(job, "github.delivery.id", delivery_id);
    assert_attribute(job, "cicd.pipeline.name", "BuildWorkflow");
    assert_attribute(job, "cicd.pipeline.task.name", "LinuxJob");
    assert_attribute(job, "cicd.pipeline.task.run.id", "41");
    assert_attribute(job, "cicd.pipeline.run.id", "31");
    assert_attribute(
        job,
        "cicd.pipeline.run.url.full",
        &format!("https://github.com/{WEBHOOK_REPOSITORY}/actions/runs/31"),
    );
    assert_attribute(job, "github.workflow.run.attempt", "2");
    assert_attribute(job, "vcs.repository.name", "webhook-private-repository");
    assert_attribute(
        job,
        "vcs.repository.url.full",
        &format!("https://github.com/{WEBHOOK_REPOSITORY}"),
    );
    assert_attribute(
        job,
        "vcs.ref.head.revision",
        &WEBHOOK_SHA_40.to_ascii_lowercase(),
    );
    assert_attribute(job, "github.workflow.event", "merge_group");
    assert_attribute(
        job,
        "github.workflow.source_branch",
        "gh-readonly-queue/main/pr-7-deadbeef",
    );
    assert_attribute(job, "github.workflow.target_branch", "main");
    assert_attribute(
        job,
        "cicd.pipeline.task.run.url.full",
        &format!("https://github.com/{WEBHOOK_REPOSITORY}/actions/runs/31/job/41"),
    );
    assert_attribute(job, "github.workflow.conclusion", "success");
    assert_attribute(job, "cicd.pipeline.task.run.result", "success");
    assert_attribute(job, "sentry.op", "github.actions.job");
    assert_attribute(job, "sentry.description", "BuildWorkflow / LinuxJob");
    assert_attribute(job, "timing_source", "reported");
    assert_i64_array_attribute(
        job,
        "github.pull_request.number",
        &(1..=20).collect::<Vec<_>>(),
    );
    assert_eq!(job.start_time_unix_nano, rfc3339_unix_nanos(job_started_at));
    assert_eq!(job.end_time_unix_nano, rfc3339_unix_nanos(job_completed_at));

    let steps = captured
        .children(job)
        .filter(|span| string_attribute(span, "sentry.op") == Some("github.actions.step"))
        .collect::<Vec<_>>();
    assert_eq!(steps.len(), 2);
    assert_eq!(steps[0].parent_span_id, job.span_id);
    assert_eq!(steps[1].parent_span_id, job.span_id);
    assert!(steps
        .iter()
        .all(|step| step.kind == OtlpSpanKind::Internal as i32));
    assert_eq!(steps[0].trace_id, job.trace_id);
    assert_eq!(steps[1].trace_id, job.trace_id);
    assert_eq!(
        steps
            .iter()
            .map(|span| string_attribute(span, "cicd.pipeline.task.run.id"))
            .collect::<Vec<_>>(),
        vec![Some("41:2"), Some("41:1")]
    );
    assert_attribute(steps[0], "cicd.pipeline.task.name", "RunTests");
    assert_attribute(steps[0], "github.workflow.conclusion", "success");
    assert_attribute(steps[0], "github.workflow.event", "merge_group");
    assert_attribute(
        steps[0],
        "github.workflow.source_branch",
        "gh-readonly-queue/main/pr-7-deadbeef",
    );
    assert_attribute(steps[0], "github.workflow.target_branch", "main");
    assert_attribute(
        steps[0],
        "cicd.pipeline.task.run.url.full",
        &format!("https://github.com/{WEBHOOK_REPOSITORY}/actions/runs/31/job/41#step:2:1"),
    );
    assert_attribute(steps[0], "cicd.pipeline.task.run.result", "success");
    assert_attribute(steps[0], "cicd.pipeline.name", "BuildWorkflow");
    assert_attribute(steps[0], "cicd.pipeline.run.id", "31");
    assert_attribute(
        steps[0],
        "vcs.repository.name",
        "webhook-private-repository",
    );
    assert_attribute(steps[0], "sentry.op", "github.actions.step");
    assert_attribute(
        steps[0],
        "sentry.description",
        "BuildWorkflow / LinuxJob / RunTests",
    );
    assert_attribute(steps[0], "timing_source", "reported");
    assert_eq!(
        steps[0].start_time_unix_nano,
        rfc3339_unix_nanos(first_step_started_at)
    );
    assert_eq!(
        steps[0].end_time_unix_nano,
        rfc3339_unix_nanos(first_step_completed_at)
    );
    assert_attribute(steps[1], "cicd.pipeline.task.name", "Checkout");
    assert_attribute(steps[1], "github.workflow.conclusion", "success");
    assert_attribute(steps[1], "github.workflow.event", "merge_group");
    assert_attribute(
        steps[1],
        "cicd.pipeline.task.run.url.full",
        &format!("https://github.com/{WEBHOOK_REPOSITORY}/actions/runs/31/job/41#step:1:1"),
    );
    assert_attribute(steps[1], "cicd.pipeline.task.run.result", "success");
    assert_attribute(steps[1], "cicd.pipeline.name", "BuildWorkflow");
    assert_attribute(steps[1], "cicd.pipeline.run.id", "31");
    assert_attribute(
        steps[1],
        "vcs.repository.name",
        "webhook-private-repository",
    );
    assert_attribute(steps[1], "sentry.op", "github.actions.step");
    assert_attribute(
        steps[1],
        "sentry.description",
        "BuildWorkflow / LinuxJob / Checkout",
    );
    assert_attribute(steps[1], "timing_source", "reported");
    assert_eq!(
        steps[1].start_time_unix_nano,
        rfc3339_unix_nanos(second_step_started_at)
    );
    assert_eq!(
        steps[1].end_time_unix_nano,
        rfc3339_unix_nanos(second_step_completed_at)
    );

    captured.assert_approved_attribute_keys();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pull_request_workflow_context_enriches_job_and_step_spans() {
    let fixture = WebhookTraceFixture::new().await;
    let run_body = serde_json::to_vec(&serde_json::json!({
        "action": "in_progress",
        "workflow_run": {
            "id": 73,
            "run_attempt": 1,
            "event": "pull_request",
            "head_branch": "feature/otlp-context",
            "pull_requests": [{
                "head": {"ref": "feature/otlp-context"},
                "base": {"ref": "main"}
            }]
        },
        "repository": {"full_name": WEBHOOK_REPOSITORY}
    }))
    .expect("workflow-run payload serializes");
    let run_response = fixture
        .webhook(
            &run_body,
            "workflow_run",
            "550e8400-e29b-41d4-a716-446655440209",
            WEBHOOK_SECRET,
        )
        .await;
    assert_eq!(run_response.status(), StatusCode::NO_CONTENT);
    drop(run_response);

    let job_body = workflow_job_body(
        Some("completed"),
        serde_json::json!({
            "id": 7301,
            "run_id": 73,
            "run_attempt": 1,
            "conclusion": "failure",
            "steps": [{"number": 4, "conclusion": "failure"}]
        }),
    );
    let job_delivery = "550e8400-e29b-41d4-a716-446655440210";
    let job_response = fixture
        .webhook(&job_body, "workflow_job", job_delivery, WEBHOOK_SECRET)
        .await;
    assert_eq!(job_response.status(), StatusCode::NO_CONTENT);
    drop(job_response);

    let captured = fixture.force_flush();
    let job = captured.workflow_job_for_delivery(job_delivery);
    let step = captured.workflow_step(job);
    for span in [job, step] {
        assert_attribute(span, "github.workflow.event", "pull_request");
        assert_attribute(
            span,
            "github.workflow.source_branch",
            "feature/otlp-context",
        );
        assert_attribute(span, "github.workflow.target_branch", "main");
    }
    assert_attribute(
        job,
        "cicd.pipeline.task.run.url.full",
        &format!("https://github.com/{WEBHOOK_REPOSITORY}/actions/runs/73/job/7301"),
    );
    assert_attribute(
        step,
        "cicd.pipeline.task.run.url.full",
        &format!("https://github.com/{WEBHOOK_REPOSITORY}/actions/runs/73/job/7301#step:4:1"),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_job_at_configured_step_limit_exports_complete_trace() {
    let fixture = WebhookTraceFixture::new_with_workflow_job_max_steps(2).await;
    let delivery_id = "550e8400-e29b-41d4-a716-446655440800";
    let body = workflow_job_body(
        Some("completed"),
        serde_json::json!({
            "id": 9900,
            "run_id": 8800,
            "run_attempt": 1,
            "conclusion": "success",
            "steps": [
                {"number": 1, "conclusion": "success"},
                {"number": 2, "conclusion": "success"}
            ]
        }),
    );

    let response = fixture
        .webhook(&body, "workflow_job", delivery_id, WEBHOOK_SECRET)
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    drop(response);

    let exposition = fixture.metrics_text().await;
    let captured = fixture.force_flush();
    let job = captured.workflow_job_for_delivery(delivery_id);
    assert_eq!(
        captured
            .children(job)
            .filter(|span| string_attribute(span, "sentry.op") == Some("github.actions.step"))
            .count(),
        2
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_steps_count{repository=\"owner/webhook-private-repository\"} 1",
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_steps_sum{repository=\"owner/webhook-private-repository\"} 2.0",
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_trace_rejections_total{repository=\"unknown\",reason=\"too_many_steps\"} 0",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_job_over_step_limit_emits_actionable_rejection_without_trace() {
    const REPOSITORY_NAME: &str = "owner/repository";
    const WORKFLOW_RUN_ID: i64 = 8801;
    const WORKFLOW_RUN_ATTEMPT: i64 = 2;
    const WORKFLOW_JOB_ID: i64 = 9901;
    const DELIVERY_ID: &str = "550e8400-e29b-41d4-a716-446655440801";
    const FORBIDDEN_WORKFLOW_NAME: &str = "rejection-workflow-name-sentinel";
    const FORBIDDEN_JOB_NAME: &str = "rejection-job-name-sentinel";
    const FORBIDDEN_STEP_NAME: &str = "rejection-step-name-sentinel";
    const FORBIDDEN_SHA: &str = "rejection-sha-sentinel";
    const FORBIDDEN_PR: &str = "rejection-pr-sentinel";
    const FORBIDDEN_ACTOR: &str = "rejection-actor-sentinel";
    const FORBIDDEN_COMMAND: &str = "secret-command";
    const FORBIDDEN_OUTPUT: &str = "secret-output";
    const FORBIDDEN_LOGS: &str = "secret-logs";
    const FORBIDDEN_RAW_URL: &str = "https://rejection-raw-url.invalid/private";
    const FORBIDDEN_DERIVED_URL: &str = "https://rejection-derived-url.invalid/private";
    const FORBIDDEN_PAYLOAD_FRAGMENT: &str = "rejection-payload-fragment-sentinel";
    const FORBIDDEN_SIGNATURE: &str = "rejection-signature-sentinel";
    const FORBIDDEN_AUTHORIZATION: &str = "rejection-authorization-sentinel";
    const FORBIDDEN_REPOSITORY_SECRET: &str = "rejection-repository-secret-sentinel";
    const FORBIDDEN_COLLECTOR_DETAILS: &str = "rejection-collector-details-sentinel";

    let fixture = WebhookTraceFixture::new_with_workflow_job_max_steps(2).await;
    let body = workflow_job_body_for_repository(
        Some("completed"),
        serde_json::json!({
            "id": WORKFLOW_JOB_ID,
            "run_id": WORKFLOW_RUN_ID,
            "run_attempt": WORKFLOW_RUN_ATTEMPT,
            "conclusion": "success",
            "workflow_name": FORBIDDEN_WORKFLOW_NAME,
            "name": FORBIDDEN_JOB_NAME,
            "head_sha": FORBIDDEN_SHA,
            "pull_requests": [{"number": 123, "sentinel": FORBIDDEN_PR}],
            "actor": {"login": FORBIDDEN_ACTOR},
            "commands": [FORBIDDEN_COMMAND],
            "output": FORBIDDEN_OUTPUT,
            "logs": FORBIDDEN_LOGS,
            "url": FORBIDDEN_RAW_URL,
            "html_url": FORBIDDEN_DERIVED_URL,
            "raw_payload_fragment": FORBIDDEN_PAYLOAD_FRAGMENT,
            "signature": FORBIDDEN_SIGNATURE,
            "authorization": FORBIDDEN_AUTHORIZATION,
            "repository_secret": FORBIDDEN_REPOSITORY_SECRET,
            "collector_details": FORBIDDEN_COLLECTOR_DETAILS,
            "steps": [
                {"number": 1, "name": FORBIDDEN_STEP_NAME, "conclusion": "success"},
                {"number": 2, "conclusion": "success"},
                {"number": 3, "conclusion": "success"}
            ]
        }),
        REPOSITORY_NAME,
    );

    let serialized_body = String::from_utf8(body.clone()).expect("webhook body is UTF-8");
    assert!(
        serialized_body.contains(FORBIDDEN_LOGS),
        "serialized webhook body contains the logs sentinel before submission"
    );
    let authorization_header = format!("Bearer {FORBIDDEN_AUTHORIZATION}");
    let response = fixture
        .webhook_with_authorization(
            &body,
            "workflow_job",
            DELIVERY_ID,
            WEBHOOK_SECRET,
            Some(&authorization_header),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    drop(response);

    let claim_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM processed_deliveries WHERE delivery_id = ?")
            .bind(DELIVERY_ID)
            .fetch_one(&fixture.pool)
            .await
            .expect("durable delivery claim is countable");
    assert_eq!(claim_count, 1);

    let exposition = fixture.metrics_text().await;
    let captured = fixture.force_flush();
    let stderr = fixture.output.text();
    assert_eq!(
        captured
            .spans
            .iter()
            .filter(|span| {
                is_workflow_task_span(span)
                    && string_attribute(span, "github.delivery.id") == Some(DELIVERY_ID)
            })
            .count(),
        0,
        "over-limit delivery emits no historical workflow spans"
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_steps_count{repository=\"owner/repository\"} 1",
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_steps_sum{repository=\"owner/repository\"} 3.0",
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_trace_rejections_total{repository=\"owner/repository\",reason=\"too_many_steps\"} 1",
    );

    for approved in [
        "reason",
        "too_many_steps",
        "repository_name",
        REPOSITORY_NAME,
        "workflow_run_id",
        "workflow_run_attempt",
        "workflow_job_id",
        "delivery_id",
        DELIVERY_ID,
        "step_count",
        "step_limit",
        "8801",
        "2",
        "9901",
        "3",
    ] {
        assert!(
            stderr.contains(approved),
            "stderr contains approved field or value {approved:?}"
        );
    }
    assert!(stderr.contains("completed workflow-job trace rejected"));
    let rejection_log = captured.workflow_rejection_log();
    assert!(
        rejection_log.trace_id.is_empty() || rejection_log.trace_id.iter().all(|byte| *byte == 0),
        "rejection warning trace ID must be absent or zero: {:?}",
        rejection_log.trace_id
    );
    assert!(
        rejection_log.span_id.is_empty() || rejection_log.span_id.iter().all(|byte| *byte == 0),
        "rejection warning span ID must be absent or zero: {:?}",
        rejection_log.span_id
    );
    let mut rejection_attribute_keys = rejection_log
        .attributes
        .iter()
        .map(|attribute| attribute.key.as_str())
        .collect::<Vec<_>>();
    rejection_attribute_keys.sort_unstable();
    assert_eq!(
        rejection_attribute_keys,
        vec![
            "delivery_id",
            "reason",
            "repository_name",
            "step_count",
            "step_limit",
            "workflow_job_id",
            "workflow_run_attempt",
            "workflow_run_id",
        ],
        "rejection warning custom OTLP attributes"
    );
    assert!(captured.has_log_body("completed workflow-job trace rejected"));
    assert!(captured.has_log_string_attribute("reason", "too_many_steps"));
    assert!(captured.has_log_string_attribute("repository_name", REPOSITORY_NAME));
    assert!(captured.has_log_string_attribute("delivery_id", DELIVERY_ID));
    for (key, value) in [
        ("workflow_run_id", WORKFLOW_RUN_ID),
        ("workflow_run_attempt", WORKFLOW_RUN_ATTEMPT),
        ("workflow_job_id", WORKFLOW_JOB_ID),
        ("step_count", 3),
        ("step_limit", 2),
    ] {
        assert!(
            captured.has_log_i64_attribute(key, value),
            "OTLP logs contain approved field {key:?}"
        );
    }

    for forbidden in [
        FORBIDDEN_WORKFLOW_NAME,
        FORBIDDEN_JOB_NAME,
        FORBIDDEN_STEP_NAME,
        FORBIDDEN_SHA,
        FORBIDDEN_PR,
        FORBIDDEN_ACTOR,
        FORBIDDEN_COMMAND,
        FORBIDDEN_OUTPUT,
        FORBIDDEN_LOGS,
        FORBIDDEN_RAW_URL,
        FORBIDDEN_DERIVED_URL,
        FORBIDDEN_PAYLOAD_FRAGMENT,
        FORBIDDEN_SIGNATURE,
        FORBIDDEN_AUTHORIZATION,
        FORBIDDEN_REPOSITORY_SECRET,
        FORBIDDEN_COLLECTOR_DETAILS,
    ] {
        captured.assert_absent(forbidden);
        captured.assert_logs_absent(forbidden);
        assert!(!stderr.contains(forbidden));
        assert!(!exposition.contains(forbidden));
    }
    assert!(exposition.contains(&format!("repository=\"{REPOSITORY_NAME}\"")));
    assert!(!exposition.contains(DELIVERY_ID));
    for identifier in [WORKFLOW_RUN_ID, WORKFLOW_JOB_ID] {
        let identifier_text = identifier.to_string();
        assert!(
            !exposition.lines().any(|line| {
                if line.starts_with('#') {
                    return false;
                }

                let Some((metric_series, _sample_value)) = line.rsplit_once(' ') else {
                    return false;
                };
                metric_series.contains(&identifier_text)
            }),
            "Prometheus metric series and labels must not contain workflow identifier {identifier}"
        );

        let identifier_value = identifier as f64;
        assert!(
            !exposition.lines().any(|line| {
                !line.starts_with('#')
                    && line
                        .rsplit_once(' ')
                        .and_then(|(_, value)| value.parse::<f64>().ok())
                        .is_some_and(|value| value == identifier_value)
            }),
            "Prometheus sample values must not contain workflow identifier {identifier}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unauthorized_and_non_completed_over_limit_jobs_skip_specialized_processing() {
    let fixture = WebhookTraceFixture::new_with_workflow_job_max_steps(2).await;
    let unauthorized_delivery = "550e8400-e29b-41d4-a716-446655440803";
    let non_completed_delivery = "550e8400-e29b-41d4-a716-446655440804";
    let over_limit_steps = serde_json::json!([
        {"number": 1},
        {"number": 2},
        {"number": 3}
    ]);
    let unauthorized_body = workflow_job_body_for_repository(
        Some("completed"),
        serde_json::json!({
            "id": 9903,
            "run_id": 8803,
            "run_attempt": 1,
            "steps": over_limit_steps.clone()
        }),
        ACTIONABLE_REPOSITORY,
    );
    let non_completed_body = workflow_job_body_for_repository(
        Some("in_progress"),
        serde_json::json!({
            "id": 9904,
            "run_id": 8804,
            "run_attempt": 1,
            "steps": over_limit_steps
        }),
        ACTIONABLE_REPOSITORY,
    );

    let unauthorized_response = fixture
        .webhook_with_signature(
            &unauthorized_body,
            "workflow_job",
            unauthorized_delivery,
            "sha256=0000000000000000000000000000000000000000000000000000000000000000",
        )
        .await;
    assert_eq!(unauthorized_response.status(), StatusCode::UNAUTHORIZED);

    let non_completed_response = fixture
        .webhook(
            &non_completed_body,
            "workflow_job",
            non_completed_delivery,
            WEBHOOK_SECRET,
        )
        .await;
    assert_eq!(non_completed_response.status(), StatusCode::NO_CONTENT);

    for (delivery_id, expected_claim_count) in [
        (unauthorized_delivery, 0_i64),
        (non_completed_delivery, 1_i64),
    ] {
        let claim_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM processed_deliveries WHERE delivery_id = ?")
                .bind(delivery_id)
                .fetch_one(&fixture.pool)
                .await
                .expect("delivery claim is countable");
        assert_eq!(claim_count, expected_claim_count, "claim for {delivery_id}");
    }

    let exposition = fixture.metrics_text().await;
    assert_metric_line(
        &exposition,
        "github_webhook_requests_total{repository=\"unknown\",result=\"unauthorized\"} 1",
    );
    assert_metric_line(
        &exposition,
        "github_webhook_requests_total{repository=\"owner/repository\",result=\"accepted\"} 1",
    );
    assert_metric_line(
        &exposition,
        "github_webhook_events_total{repository=\"owner/repository\",event_type=\"workflow_job\",action=\"in_progress\"} 1",
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_steps_count{repository=\"unknown\"} 0",
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_steps_sum{repository=\"unknown\"} 0.0",
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_trace_rejections_total{repository=\"unknown\",reason=\"too_many_steps\"} 0",
    );

    let captured = fixture.force_flush();
    let stderr = fixture.output.text();
    for delivery_id in [unauthorized_delivery, non_completed_delivery] {
        assert_eq!(
            captured
                .spans
                .iter()
                .filter(|span| {
                    is_workflow_task_span(span)
                        && string_attribute(span, "github.delivery.id") == Some(delivery_id)
                })
                .count(),
            0,
            "non-admitted delivery {delivery_id} emits no historical workflow spans"
        );
    }
    assert!(!stderr.contains("completed workflow-job trace rejected"));
    captured.assert_logs_absent("completed workflow-job trace rejected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_workflow_admission_after_authentication_has_no_specialized_effects() {
    let fixture = WebhookTraceFixture::new_with_workflow_job_max_steps(2).await;
    let delivery_id = "550e8400-e29b-41d4-a716-446655440805";
    let body = workflow_job_body_for_repository(
        Some("completed"),
        serde_json::Value::Null,
        ACTIONABLE_REPOSITORY,
    );

    let response = fixture
        .webhook(&body, "workflow_job", delivery_id, WEBHOOK_SECRET)
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let claim_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM processed_deliveries WHERE delivery_id = ?")
            .bind(delivery_id)
            .fetch_one(&fixture.pool)
            .await
            .expect("delivery claim is countable");
    assert_eq!(claim_count, 1);

    let exposition = fixture.metrics_text().await;
    assert_metric_line(
        &exposition,
        "github_webhook_requests_total{repository=\"owner/repository\",result=\"accepted\"} 1",
    );
    assert_metric_line(
        &exposition,
        "github_webhook_events_total{repository=\"owner/repository\",event_type=\"workflow_job\",action=\"completed\"} 1",
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_steps_count{repository=\"unknown\"} 0",
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_steps_sum{repository=\"unknown\"} 0.0",
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_trace_rejections_total{repository=\"unknown\",reason=\"too_many_steps\"} 0",
    );

    let captured = fixture.force_flush();
    let stderr = fixture.output.text();
    assert_eq!(
        captured
            .spans
            .iter()
            .filter(|span| {
                is_workflow_task_span(span)
                    && string_attribute(span, "github.delivery.id") == Some(delivery_id)
            })
            .count(),
        0,
        "malformed admission emits no historical workflow spans"
    );
    assert!(!stderr.contains("completed workflow-job trace rejected"));
    captured.assert_logs_absent("completed workflow-job trace rejected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn malformed_detailed_workflow_projection_observes_admission_once_without_rejection() {
    let fixture = WebhookTraceFixture::new_with_workflow_job_max_steps(2).await;
    let delivery_id = "550e8400-e29b-41d4-a716-446655440806";
    let body = workflow_job_body_for_repository(
        Some("completed"),
        serde_json::json!({
            "id": 9906,
            "run_id": 8806,
            "run_attempt": 1,
            "steps": [{"name": "missing required number"}]
        }),
        ACTIONABLE_REPOSITORY,
    );

    let response = fixture
        .webhook(&body, "workflow_job", delivery_id, WEBHOOK_SECRET)
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let claim_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM processed_deliveries WHERE delivery_id = ?")
            .bind(delivery_id)
            .fetch_one(&fixture.pool)
            .await
            .expect("delivery claim is countable");
    assert_eq!(claim_count, 1);

    let exposition = fixture.metrics_text().await;
    assert_metric_line(
        &exposition,
        "github_webhook_requests_total{repository=\"owner/repository\",result=\"accepted\"} 1",
    );
    assert_metric_line(
        &exposition,
        "github_webhook_events_total{repository=\"owner/repository\",event_type=\"workflow_job\",action=\"completed\"} 1",
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_steps_count{repository=\"owner/repository\"} 1",
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_steps_sum{repository=\"owner/repository\"} 1.0",
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_trace_rejections_total{repository=\"unknown\",reason=\"too_many_steps\"} 0",
    );

    let captured = fixture.force_flush();
    let stderr = fixture.output.text();
    assert_eq!(
        captured
            .spans
            .iter()
            .filter(|span| {
                is_workflow_task_span(span)
                    && string_attribute(span, "github.delivery.id") == Some(delivery_id)
            })
            .count(),
        0,
        "detailed projection failure emits no historical workflow spans"
    );
    assert!(!stderr.contains("completed workflow-job trace rejected"));
    captured.assert_logs_absent("completed workflow-job trace rejected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_identifiers_and_names_are_span_only_and_payload_data_is_absent() {
    let required_workflow_span_only_keys = [
        "cicd.pipeline.name",
        "cicd.pipeline.run.id",
        "cicd.pipeline.run.url.full",
        "cicd.pipeline.task.name",
        "cicd.pipeline.task.run.id",
        "cicd.pipeline.task.run.url.full",
        "sentry.description",
        "vcs.ref.head.revision",
        "vcs.repository.name",
        "vcs.repository.url.full",
        "github.workflow.run.attempt",
    ];
    for key in required_workflow_span_only_keys {
        assert!(
            SPAN_ONLY_ATTRIBUTE_KEYS.contains(&key),
            "workflow span-only attribute key {key:?} is centralized"
        );
    }

    let fixture = WebhookTraceFixture::new().await;
    let body = workflow_job_body(
        Some("completed"),
        serde_json::json!({
            "id": WORKFLOW_PRIVACY_JOB_ID,
            "run_id": WORKFLOW_PRIVACY_RUN_ID,
            "run_attempt": WORKFLOW_PRIVACY_RUN_ATTEMPT,
            "workflow_name": WORKFLOW_RAW_NAME,
            "name": WORKFLOW_RAW_JOB_NAME,
            "conclusion": WORKFLOW_UNKNOWN_CONCLUSION,
            "head_sha": WORKFLOW_PRIVACY_SHA,
            "started_at": "2026-08-06T13:00:00.123456789Z",
            "completed_at": "2026-08-06T13:05:00.987654321Z",
            "pull_requests": WORKFLOW_PRIVACY_PR_NUMBERS
                .iter()
                .map(|number| serde_json::json!({"number": number}))
                .collect::<Vec<_>>(),
            "command": WORKFLOW_FORBIDDEN_COMMAND,
            "output": WORKFLOW_FORBIDDEN_OUTPUT,
            "logs": WORKFLOW_FORBIDDEN_LOG,
            "actor": {"login": WORKFLOW_FORBIDDEN_ACTOR},
            "url": WORKFLOW_FORBIDDEN_URL,
            "secret": WORKFLOW_FORBIDDEN_SECRET,
            "signature": WORKFLOW_FORBIDDEN_SIGNATURE,
            "headers": {"x-task6-private": WORKFLOW_FORBIDDEN_HEADER},
            "raw_payload_fragment": WORKFLOW_FORBIDDEN_FRAGMENT,
            "steps": [{
                "number": WORKFLOW_PRIVACY_STEP_NUMBER,
                "name": WORKFLOW_RAW_STEP_NAME,
                "conclusion": WORKFLOW_UNKNOWN_CONCLUSION,
                "started_at": "2026-08-06T13:01:00.111111111Z",
                "completed_at": "2026-08-06T13:02:00.222222222Z",
                "command": WORKFLOW_FORBIDDEN_COMMAND,
                "output": WORKFLOW_FORBIDDEN_OUTPUT,
                "logs": WORKFLOW_FORBIDDEN_LOG,
                "actor": WORKFLOW_FORBIDDEN_ACTOR,
                "url": WORKFLOW_FORBIDDEN_URL
            }]
        }),
    );

    let response = fixture
        .webhook(
            &body,
            "workflow_job",
            WORKFLOW_PRIVACY_DELIVERY,
            WEBHOOK_SECRET,
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(to_bytes(response.into_body(), 1)
        .await
        .expect("workflow response body is readable")
        .is_empty());

    let exposition = fixture.metrics_text().await;
    let captured = fixture.force_flush();
    let stderr = fixture.output.text();
    let job = captured.workflow_job_for_delivery(WORKFLOW_PRIVACY_DELIVERY);
    let task_run_id = format!("{WORKFLOW_PRIVACY_JOB_ID}:{WORKFLOW_PRIVACY_STEP_NUMBER}");
    let step = captured.workflow_step_for_task_run_id(job, &task_run_id);

    assert!(job.parent_span_id.is_empty());
    assert_attribute(job, "github.delivery.id", WORKFLOW_PRIVACY_DELIVERY);
    assert_attribute(job, "vcs.repository.name", "webhook-private-repository");
    assert_attribute(job, "vcs.ref.head.revision", WORKFLOW_PRIVACY_SHA);
    assert_attribute(job, "cicd.pipeline.name", WORKFLOW_SANITIZED_NAME);
    assert_attribute(job, "cicd.pipeline.task.name", WORKFLOW_SANITIZED_JOB_NAME);
    assert_attribute(
        job,
        "cicd.pipeline.run.id",
        &WORKFLOW_PRIVACY_RUN_ID.to_string(),
    );
    assert_attribute(
        job,
        "github.workflow.run.attempt",
        &WORKFLOW_PRIVACY_RUN_ATTEMPT.to_string(),
    );
    assert_attribute(
        job,
        "cicd.pipeline.task.run.id",
        &WORKFLOW_PRIVACY_JOB_ID.to_string(),
    );
    assert_i64_array_attribute(
        job,
        "github.pull_request.number",
        WORKFLOW_PRIVACY_PR_NUMBERS,
    );
    assert_attribute(job, "github.workflow.conclusion", "other");
    assert_attribute(job, "cicd.pipeline.task.run.result", "other");
    assert_eq!(string_attribute(job, "error.type"), None);
    assert_historical_interval(
        job,
        rfc3339_unix_nanos("2026-08-06T13:00:00.123456789Z"),
        rfc3339_unix_nanos("2026-08-06T13:05:00.987654321Z"),
        "reported",
    );
    assert_attribute(
        step,
        "cicd.pipeline.task.name",
        WORKFLOW_SANITIZED_STEP_NAME,
    );
    assert_attribute(step, "cicd.pipeline.task.run.id", &task_run_id);
    assert_attribute(step, "github.workflow.conclusion", "other");
    assert_attribute(step, "cicd.pipeline.task.run.result", "other");
    assert_eq!(string_attribute(step, "error.type"), None);
    assert_historical_interval(
        step,
        rfc3339_unix_nanos("2026-08-06T13:01:00.111111111Z"),
        rfc3339_unix_nanos("2026-08-06T13:02:00.222222222Z"),
        "reported",
    );

    captured.assert_approved_attribute_keys();
    for key in required_workflow_span_only_keys {
        assert!(
            captured.has_trace_attribute_key(key),
            "workflow span-only attribute key {key:?} is present in traces"
        );
    }
    for key in SPAN_ONLY_ATTRIBUTE_KEYS {
        assert!(
            !captured.has_log_attribute_key(key),
            "OTLP logs must not contain span-only attribute key {key:?}"
        );
    }
    assert!(captured
        .has_trace_i64_array_attribute("github.pull_request.number", WORKFLOW_PRIVACY_PR_NUMBERS,));
    assert!(!captured
        .has_log_i64_array_attribute("github.pull_request.number", WORKFLOW_PRIVACY_PR_NUMBERS,));
    for numeric_identifier in [
        WORKFLOW_PRIVACY_RUN_ID,
        WORKFLOW_PRIVACY_RUN_ATTEMPT,
        WORKFLOW_PRIVACY_JOB_ID,
        WORKFLOW_PRIVACY_PR_NUMBERS[0],
        WORKFLOW_PRIVACY_PR_NUMBERS[1],
    ] {
        assert!(
            !captured.has_log_i64_value(numeric_identifier),
            "OTLP log bodies, attributes, and resources must not contain numeric workflow identifier {numeric_identifier}"
        );
        let value = numeric_identifier.to_string();
        assert!(!stderr.contains(&value));
        assert!(!exposition.contains(&value));
    }

    let run_id = WORKFLOW_PRIVACY_RUN_ID.to_string();
    let run_attempt = WORKFLOW_PRIVACY_RUN_ATTEMPT.to_string();
    let job_id = WORKFLOW_PRIVACY_JOB_ID.to_string();
    assert!(exposition.contains(&format!("repository=\"{WEBHOOK_REPOSITORY}\"")));
    captured.assert_logs_absent(WEBHOOK_REPOSITORY);
    assert!(!stderr.contains(WEBHOOK_REPOSITORY));
    for approved_span_only_value in [
        WORKFLOW_PRIVACY_DELIVERY,
        WORKFLOW_PRIVACY_SHA,
        WORKFLOW_SANITIZED_NAME,
        WORKFLOW_SANITIZED_JOB_NAME,
        WORKFLOW_SANITIZED_STEP_NAME,
        run_id.as_str(),
        run_attempt.as_str(),
        job_id.as_str(),
        task_run_id.as_str(),
    ] {
        captured.assert_logs_absent(approved_span_only_value);
        assert!(
            !stderr.contains(approved_span_only_value),
            "stderr must not contain span-only value {approved_span_only_value:?}"
        );
        assert!(
            !exposition.contains(approved_span_only_value),
            "Prometheus must not contain span-only value {approved_span_only_value:?}"
        );
    }

    for forbidden in [
        WORKFLOW_RAW_NAME,
        WORKFLOW_ESCAPED_RAW_NAME,
        WORKFLOW_RAW_JOB_NAME,
        WORKFLOW_ESCAPED_RAW_JOB_NAME,
        WORKFLOW_RAW_STEP_NAME,
        WORKFLOW_ESCAPED_RAW_STEP_NAME,
        WORKFLOW_FORBIDDEN_COMMAND,
        WORKFLOW_FORBIDDEN_OUTPUT,
        WORKFLOW_FORBIDDEN_LOG,
        WORKFLOW_FORBIDDEN_ACTOR,
        WORKFLOW_FORBIDDEN_URL,
        WORKFLOW_FORBIDDEN_SECRET,
        WORKFLOW_FORBIDDEN_SIGNATURE,
        WORKFLOW_FORBIDDEN_HEADER,
        WORKFLOW_FORBIDDEN_FRAGMENT,
        WORKFLOW_UNKNOWN_CONCLUSION,
    ] {
        captured.assert_absent(forbidden);
        captured.assert_logs_absent(forbidden);
        assert!(
            !stderr.contains(forbidden),
            "stderr must not contain forbidden workflow value {forbidden:?}"
        );
        assert!(
            !exposition.contains(forbidden),
            "Prometheus must not contain forbidden workflow value {forbidden:?}"
        );
    }
    for expected in [
        "github_webhook_requests_total{repository=\"owner/webhook-private-repository\",result=\"accepted\"} 1",
        "github_webhook_events_total{repository=\"owner/webhook-private-repository\",event_type=\"workflow_job\",action=\"completed\"} 1",
        "github_webhook_request_body_bytes_count{repository=\"owner/webhook-private-repository\"} 1",
        "github_webhook_duplicates_total{repository=\"unknown\"} 0",
    ] {
        assert_metric_line(&exposition, expected);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hostile_failed_workflow_payload_is_private() {
    let raw_step_name = format!("\n{}", "x".repeat(140));
    let escaped_raw_step_name = format!("\\n{}", "x".repeat(140));
    let sanitized_step_name = "x".repeat(128);
    let expected_message = format!("CI task failed: {sanitized_step_name}");
    let completed_at = "2026-08-06T13:02:00.222222222Z";
    let (fixture, sentry_transport) = WebhookTraceFixture::new_with_sentry().await;
    let body = workflow_job_body(
        Some("completed"),
        serde_json::json!({
            "id": WORKFLOW_PRIVACY_JOB_ID,
            "run_id": WORKFLOW_PRIVACY_RUN_ID,
            "run_attempt": WORKFLOW_PRIVACY_RUN_ATTEMPT,
            "workflow_name": WORKFLOW_RAW_NAME,
            "name": WORKFLOW_RAW_JOB_NAME,
            "conclusion": "failure",
            "head_sha": WORKFLOW_PRIVACY_SHA,
            "started_at": "2026-08-06T13:00:00.123456789Z",
            "completed_at": "2026-08-06T13:05:00.987654321Z",
            "command": WORKFLOW_FORBIDDEN_COMMAND,
            "output": WORKFLOW_FORBIDDEN_OUTPUT,
            "logs": WORKFLOW_FORBIDDEN_LOG,
            "actor": {"login": WORKFLOW_FORBIDDEN_ACTOR},
            "url": WORKFLOW_FORBIDDEN_URL,
            "secret": WORKFLOW_FORBIDDEN_SECRET,
            "signature": WORKFLOW_FORBIDDEN_SIGNATURE,
            "headers": {"x-task6-private": WORKFLOW_FORBIDDEN_HEADER},
            "raw_payload_fragment": WORKFLOW_FORBIDDEN_FRAGMENT,
            "unknown_conclusion": WORKFLOW_UNKNOWN_CONCLUSION,
            "steps": [{
                "number": WORKFLOW_PRIVACY_STEP_NUMBER,
                "name": raw_step_name.clone(),
                "conclusion": "failure",
                "started_at": "2026-08-06T13:01:00.111111111Z",
                "completed_at": completed_at,
                "command": WORKFLOW_FORBIDDEN_COMMAND,
                "output": WORKFLOW_FORBIDDEN_OUTPUT,
                "logs": WORKFLOW_FORBIDDEN_LOG,
                "actor": WORKFLOW_FORBIDDEN_ACTOR,
                "url": WORKFLOW_FORBIDDEN_URL,
                "secret": WORKFLOW_FORBIDDEN_SECRET
            }]
        }),
    );

    let response = fixture
        .webhook_with_authorization(
            &body,
            "workflow_job",
            WORKFLOW_PRIVACY_DELIVERY,
            WEBHOOK_SECRET,
            Some(AUTHORIZATION),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let captured = fixture.force_flush();
    let task_run_id = format!("{WORKFLOW_PRIVACY_JOB_ID}:{WORKFLOW_PRIVACY_STEP_NUMBER}");
    let job = captured.workflow_job_for_delivery(WORKFLOW_PRIVACY_DELIVERY);
    let step = captured.workflow_step_for_task_run_id(job, &task_run_id);
    assert_no_exception_events(job);
    assert_exception_event(
        step,
        "GitHubActionsTaskFailure",
        &expected_message,
        rfc3339_unix_nanos(completed_at),
    );
    assert_otlp_status(job, OtlpStatusCode::Error, "workflow_failed");
    assert_otlp_status(step, OtlpStatusCode::Error, "workflow_failed");
    assert_eq!(step.trace_id, job.trace_id);
    assert_eq!(step.parent_span_id, job.span_id);

    let events = sentry_transport.fetch_and_clear_events();
    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert_eq!(event.level, SentryLevel::Error);
    assert_eq!(
        event
            .fingerprint
            .iter()
            .map(AsRef::as_ref)
            .collect::<Vec<_>>(),
        [
            "github-actions-task",
            "step",
            WEBHOOK_REPOSITORY,
            WORKFLOW_SANITIZED_NAME,
            WORKFLOW_SANITIZED_JOB_NAME,
            sanitized_step_name.as_str(),
            "failure",
        ]
    );
    assert_eq!(event.exception.values.len(), 1);
    let exception = &event.exception.values[0];
    assert_eq!(exception.ty, "GitHubActionsTaskFailure");
    assert_eq!(exception.value.as_deref(), Some(expected_message.as_str()));
    assert!(exception.module.is_none());
    assert!(exception.stacktrace.is_none());
    assert!(exception.raw_stacktrace.is_none());
    assert!(exception.thread_id.is_none());
    let mechanism = exception.mechanism.as_ref().expect("mechanism is present");
    assert_eq!(mechanism.ty, "github_actions");
    assert_eq!(mechanism.handled, Some(true));
    assert_eq!(mechanism.synthetic, None);
    assert_eq!(event.contexts.len(), 1);
    match event.contexts.get("trace") {
        Some(SentryContext::Trace(trace)) => {
            assert_eq!(trace.trace_id.to_string(), hex::encode(&step.trace_id));
            assert_eq!(trace.span_id.to_string(), hex::encode(&step.span_id));
            assert_eq!(trace.op.as_deref(), Some("github.actions.step"));
            assert_eq!(
                trace.description.as_deref(),
                Some(
                    format!(
                        "{WORKFLOW_SANITIZED_NAME} / {WORKFLOW_SANITIZED_JOB_NAME} / {sanitized_step_name}"
                    )
                    .as_str()
                )
            );
            assert_eq!(trace.status, Some(SentrySpanStatus::InternalError));
            assert_eq!(trace.origin.as_deref(), Some("manual.github.workflow"));
        }
        Some(other) => panic!("unexpected Sentry context: {other:?}"),
        None => panic!("trace context is present"),
    }
    assert_eq!(event.tags.len(), 5);
    for (key, expected) in [
        ("github.repository.name", WEBHOOK_REPOSITORY),
        ("cicd.pipeline.name", WORKFLOW_SANITIZED_NAME),
        ("cicd.pipeline.task.name", sanitized_step_name.as_str()),
        ("cicd.pipeline.task.run.id", task_run_id.as_str()),
        ("github.workflow.conclusion", "failure"),
    ] {
        assert_eq!(event.tags.get(key).map(String::as_str), Some(expected));
    }
    assert_eq!(
        event.server_name.as_deref(),
        Some("github-webhook-exporter-test")
    );
    assert_eq!(event.release.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    assert_eq!(event.platform.as_ref(), "native");
    assert!(event.culprit.is_none());
    assert!(event.transaction.is_none());
    assert!(event.message.is_none());
    assert!(event.logentry.is_none());
    assert!(event.logger.is_none());
    assert!(event.dist.is_none());
    assert!(event.environment.is_none());
    assert!(event.user.is_none());
    assert!(event.request.is_none());
    assert!(event.breadcrumbs.is_empty());
    assert!(event.modules.is_empty());
    assert!(event.extra.is_empty());
    assert!(event.stacktrace.is_none());
    assert!(event.threads.is_empty());
    let sentry_timestamp = u64::try_from(
        event
            .timestamp
            .duration_since(std::time::SystemTime::UNIX_EPOCH)
            .expect("historical timestamp follows the Unix epoch")
            .as_nanos(),
    )
    .expect("historical timestamp fits u64 nanoseconds");
    assert_eq!(sentry_timestamp, rfc3339_unix_nanos(completed_at));

    let serialized_sentry = serde_json::to_string(event).expect("Sentry event serializes");
    for forbidden in [
        WORKFLOW_RAW_NAME,
        WORKFLOW_ESCAPED_RAW_NAME,
        WORKFLOW_RAW_JOB_NAME,
        WORKFLOW_ESCAPED_RAW_JOB_NAME,
        raw_step_name.as_str(),
        escaped_raw_step_name.as_str(),
        WORKFLOW_FORBIDDEN_COMMAND,
        WORKFLOW_FORBIDDEN_OUTPUT,
        WORKFLOW_FORBIDDEN_LOG,
        WORKFLOW_FORBIDDEN_ACTOR,
        WORKFLOW_FORBIDDEN_URL,
        WORKFLOW_FORBIDDEN_SECRET,
        WORKFLOW_FORBIDDEN_SIGNATURE,
        WORKFLOW_FORBIDDEN_HEADER,
        WORKFLOW_FORBIDDEN_FRAGMENT,
        WORKFLOW_UNKNOWN_CONCLUSION,
        WEBHOOK_SECRET,
        AUTHORIZATION,
    ] {
        captured.assert_absent(forbidden);
        assert!(
            !serialized_sentry.contains(forbidden),
            "serialized Sentry event must not contain {forbidden:?}"
        );
    }
    captured.assert_approved_attribute_keys();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_conclusions_export_bounded_results_and_statuses() {
    type ExpectedExceptionEvent = (&'static str, &'static str, &'static str);
    type WorkflowConclusionCase = (
        &'static str,
        &'static str,
        &'static str,
        Option<&'static str>,
        OtlpStatusCode,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        Option<ExpectedExceptionEvent>,
        Option<ExpectedExceptionEvent>,
    );

    let fixture = WebhookTraceFixture::new().await;
    let cases: [WorkflowConclusionCase; 7] = [
        (
            "550e8400-e29b-41d4-a716-446655440300",
            "success",
            "success",
            Some("success"),
            OtlpStatusCode::Ok,
            "",
            "Success Job",
            "Success Step",
            "2026-08-06T10:00:01.000000002Z",
            "2026-08-06T10:00:00.900000004Z",
            None,
            None,
        ),
        (
            "550e8400-e29b-41d4-a716-446655440301",
            "failure",
            "failure",
            Some("failure"),
            OtlpStatusCode::Error,
            "workflow_failed",
            "Failure Job",
            "Failure Step",
            "2026-08-06T10:10:01.000000002Z",
            "2026-08-06T10:10:00.900000004Z",
            None,
            Some((
                "GitHubActionsTaskFailure",
                "CI task failed: Failure Step",
                "2026-08-06T10:10:00.900000004Z",
            )),
        ),
        (
            "550e8400-e29b-41d4-a716-446655440302",
            "cancelled",
            "cancelled",
            Some("cancellation"),
            OtlpStatusCode::Unset,
            "",
            "Cancelled Job",
            "Cancelled Step",
            "2026-08-06T10:20:01.000000002Z",
            "2026-08-06T10:20:00.900000004Z",
            None,
            None,
        ),
        (
            "550e8400-e29b-41d4-a716-446655440303",
            "skipped",
            "skipped",
            Some("skip"),
            OtlpStatusCode::Unset,
            "",
            "Skipped Job",
            "Skipped Step",
            "2026-08-06T10:30:01.000000002Z",
            "2026-08-06T10:30:00.900000004Z",
            None,
            None,
        ),
        (
            "550e8400-e29b-41d4-a716-446655440304",
            "timed_out",
            "timed_out",
            Some("timeout"),
            OtlpStatusCode::Error,
            "workflow_failed",
            "Timed Out Job",
            "Timed Out Step",
            "2026-08-06T10:40:01.000000002Z",
            "2026-08-06T10:40:00.900000004Z",
            None,
            Some((
                "GitHubActionsTaskTimeout",
                "CI task timed out: Timed Out Step",
                "2026-08-06T10:40:00.900000004Z",
            )),
        ),
        (
            "550e8400-e29b-41d4-a716-446655440305",
            "neutral",
            "neutral",
            Some("neutral"),
            OtlpStatusCode::Unset,
            "",
            "Neutral Job",
            "Neutral Step",
            "2026-08-06T10:50:01.000000002Z",
            "2026-08-06T10:50:00.900000004Z",
            None,
            None,
        ),
        (
            "550e8400-e29b-41d4-a716-446655440306",
            "fixture_private_unknown",
            "other",
            Some("other"),
            OtlpStatusCode::Unset,
            "",
            "Other Job",
            "Other Step",
            "2026-08-06T11:00:01.000000002Z",
            "2026-08-06T11:00:00.900000004Z",
            None,
            None,
        ),
    ];
    let fallback_delivery_id = "550e8400-e29b-41d4-a716-446655440307";
    let fallback_job_completed_at = "2026-08-06T11:10:01.000000002Z";
    let fallback_step_completed_at = "2026-08-06T11:10:00.900000004Z";

    for (
        index,
        (
            delivery_id,
            raw,
            _,
            _,
            _,
            _,
            job_name,
            step_name,
            job_completed_at,
            step_completed_at,
            _,
            _,
        ),
    ) in cases.iter().enumerate()
    {
        let body = workflow_job_body(
            Some("completed"),
            serde_json::json!({
                "id": 100 + index,
                "run_id": 200 + index,
                "run_attempt": 1,
                "name": job_name,
                "conclusion": raw,
                "started_at": "2026-08-06T10:00:00.000000001Z",
                "completed_at": job_completed_at,
                "steps": [{
                    "number": 1,
                    "name": step_name,
                    "conclusion": raw,
                    "started_at": "2026-08-06T10:00:00.100000003Z",
                    "completed_at": step_completed_at
                }]
            }),
        );
        let response = fixture
            .webhook(&body, "workflow_job", delivery_id, WEBHOOK_SECRET)
            .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    let fallback_body = workflow_job_body(
        Some("completed"),
        serde_json::json!({
            "id": 107,
            "run_id": 207,
            "run_attempt": 1,
            "name": "Fallback Failure Job",
            "conclusion": "failure",
            "started_at": "2026-08-06T11:10:00.000000001Z",
            "completed_at": fallback_job_completed_at,
            "steps": [{
                "number": 1,
                "name": "Successful Child",
                "conclusion": "success",
                "started_at": "2026-08-06T11:10:00.100000003Z",
                "completed_at": fallback_step_completed_at
            }]
        }),
    );
    let fallback_response = fixture
        .webhook(
            &fallback_body,
            "workflow_job",
            fallback_delivery_id,
            WEBHOOK_SECRET,
        )
        .await;
    assert_eq!(fallback_response.status(), StatusCode::NO_CONTENT);

    let captured = fixture.force_flush();
    assert_eq!(
        captured
            .spans
            .iter()
            .filter(|span| string_attribute(span, "sentry.op") == Some("github.actions.job"))
            .count(),
        cases.len() + 1
    );
    assert_eq!(
        captured
            .spans
            .iter()
            .filter(|span| string_attribute(span, "sentry.op") == Some("github.actions.step"))
            .count(),
        cases.len() + 1
    );

    for (
        delivery_id,
        _,
        normalized,
        semantic_result,
        status,
        description,
        _,
        _,
        _,
        _,
        expected_job_event,
        expected_step_event,
    ) in &cases
    {
        let job = captured.workflow_job_for_delivery(delivery_id);
        let step = captured.workflow_step(job);

        assert_attribute(job, "github.workflow.conclusion", normalized);
        assert_attribute(step, "github.workflow.conclusion", normalized);
        assert_eq!(
            string_attribute(job, "cicd.pipeline.task.run.result"),
            *semantic_result
        );
        assert_eq!(
            string_attribute(step, "cicd.pipeline.task.run.result"),
            *semantic_result
        );
        let expected_error_type = match *normalized {
            "failure" => Some("GitHubActionsTaskFailure"),
            "timed_out" => Some("GitHubActionsTaskTimeout"),
            "success" | "cancelled" | "skipped" | "neutral" | "other" => None,
            unexpected => panic!("unexpected normalized conclusion {unexpected}"),
        };
        assert_eq!(string_attribute(job, "error.type"), expected_error_type);
        assert_eq!(string_attribute(step, "error.type"), expected_error_type);
        // These protobuf assertions exercise WorkflowConclusion::status.
        assert_otlp_status(job, *status, description);
        assert_otlp_status(step, *status, description);
        match expected_job_event {
            Some((expected_type, expected_message, expected_timestamp)) => assert_exception_event(
                job,
                expected_type,
                expected_message,
                rfc3339_unix_nanos(expected_timestamp),
            ),
            None => assert_no_exception_events(job),
        }
        match expected_step_event {
            Some((expected_type, expected_message, expected_timestamp)) => assert_exception_event(
                step,
                expected_type,
                expected_message,
                rfc3339_unix_nanos(expected_timestamp),
            ),
            None => assert_no_exception_events(step),
        }
    }

    let fallback_job = captured.workflow_job_for_delivery(fallback_delivery_id);
    let fallback_step = captured.workflow_step(fallback_job);
    assert_attribute(fallback_job, "github.workflow.conclusion", "failure");
    assert_attribute(fallback_step, "github.workflow.conclusion", "success");
    assert_eq!(
        string_attribute(fallback_job, "cicd.pipeline.task.run.result"),
        Some("failure")
    );
    assert_eq!(
        string_attribute(fallback_step, "cicd.pipeline.task.run.result"),
        Some("success")
    );
    assert_otlp_status(fallback_job, OtlpStatusCode::Error, "workflow_failed");
    assert_otlp_status(fallback_step, OtlpStatusCode::Ok, "");
    assert_exception_event(
        fallback_job,
        "GitHubActionsTaskFailure",
        "CI task failed: Fallback Failure Job",
        rfc3339_unix_nanos(fallback_job_completed_at),
    );
    assert_no_exception_events(fallback_step);
    captured.assert_absent("fixture_private_unknown");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workflow_timing_uses_reported_and_bounded_fallback_intervals() {
    let fixture = WebhookTraceFixture::new().await;
    let reported_delivery = "550e8400-e29b-41d4-a716-446655440400";
    let malformed_completion_delivery = "550e8400-e29b-41d4-a716-446655440401";
    let missing_delivery = "550e8400-e29b-41d4-a716-446655440402";
    let malformed_receipt_delivery = "550e8400-e29b-41d4-a716-446655440403";
    let reversed_delivery = "550e8400-e29b-41d4-a716-446655440404";
    let job_started_at = "2026-08-06T10:00:00.123456789Z";
    let job_completed_at = "2026-08-06T10:05:00.987654321Z";
    let step_started_at = "2026-08-06T10:01:00.111111111Z";
    let step_completed_at = "2026-08-06T10:02:00.222222222Z";
    let reported_body = workflow_job_body(
        Some("completed"),
        serde_json::json!({
            "id": 410,
            "run_id": 310,
            "run_attempt": 2,
            "started_at": job_started_at,
            "completed_at": job_completed_at,
            "steps": [
                {
                    "number": 1,
                    "started_at": step_started_at,
                    "completed_at": step_completed_at
                },
                {
                    "number": 2,
                    "started_at": "not-a-timestamp",
                    "completed_at": "also-not-a-timestamp"
                },
                {"number": 3},
                {
                    "number": 4,
                    "started_at": "2026-08-06T10:03:00.000000004Z",
                    "completed_at": "2026-08-06T10:02:00.000000005Z"
                },
                {
                    "number": 5,
                    "started_at": "2026-08-06T09:59:59.000000006Z",
                    "completed_at": "2026-08-06T10:01:00.000000007Z"
                }
            ]
        }),
    );
    let response = fixture
        .webhook(
            &reported_body,
            "workflow_job",
            reported_delivery,
            WEBHOOK_SECRET,
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let malformed_completion_at = "2026-08-06T11:05:00.333333333Z";
    let malformed_completion_body = workflow_job_body(
        Some("completed"),
        serde_json::json!({
            "id": 411,
            "run_id": 311,
            "run_attempt": 1,
            "started_at": "not-a-timestamp",
            "completed_at": malformed_completion_at,
            "steps": []
        }),
    );
    let response = fixture
        .webhook(
            &malformed_completion_body,
            "workflow_job",
            malformed_completion_delivery,
            WEBHOOK_SECRET,
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let missing_body = workflow_job_body(
        Some("completed"),
        serde_json::json!({
            "id": 412,
            "run_id": 312,
            "run_attempt": 1,
            "steps": []
        }),
    );
    let missing_request_started = current_unix_nanos();
    let response = fixture
        .webhook(
            &missing_body,
            "workflow_job",
            missing_delivery,
            WEBHOOK_SECRET,
        )
        .await;
    let missing_request_completed = current_unix_nanos();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let malformed_receipt_body = workflow_job_body(
        Some("completed"),
        serde_json::json!({
            "id": 413,
            "run_id": 313,
            "run_attempt": 1,
            "started_at": "not-a-timestamp",
            "completed_at": {"invalid": true},
            "steps": []
        }),
    );
    let malformed_request_started = current_unix_nanos();
    let response = fixture
        .webhook(
            &malformed_receipt_body,
            "workflow_job",
            malformed_receipt_delivery,
            WEBHOOK_SECRET,
        )
        .await;
    let malformed_request_completed = current_unix_nanos();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let reversed_completion_at = "2026-08-06T12:04:00.444444444Z";
    let reversed_body = workflow_job_body(
        Some("completed"),
        serde_json::json!({
            "id": 414,
            "run_id": 314,
            "run_attempt": 1,
            "started_at": "2026-08-06T12:05:00.555555555Z",
            "completed_at": reversed_completion_at,
            "steps": []
        }),
    );
    let response = fixture
        .webhook(
            &reversed_body,
            "workflow_job",
            reversed_delivery,
            WEBHOOK_SECRET,
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let captured = fixture.force_flush();
    let reported_job = captured.workflow_job_for_delivery(reported_delivery);
    let reported_job_end = rfc3339_unix_nanos(job_completed_at);
    assert_historical_interval(
        reported_job,
        rfc3339_unix_nanos(job_started_at),
        reported_job_end,
        "reported",
    );
    let steps = captured
        .children(reported_job)
        .filter(|span| string_attribute(span, "sentry.op") == Some("github.actions.step"))
        .collect::<Vec<_>>();
    assert_eq!(steps.len(), 5);
    let step_by_number = |number: i64| {
        let task_run_id = format!("410:{number}");
        steps
            .iter()
            .copied()
            .find(|span| {
                string_attribute(span, "cicd.pipeline.task.run.id") == Some(task_run_id.as_str())
            })
            .unwrap_or_else(|| panic!("workflow step {task_run_id} is exported"))
    };
    assert_historical_interval(
        step_by_number(1),
        rfc3339_unix_nanos(step_started_at),
        rfc3339_unix_nanos(step_completed_at),
        "reported",
    );
    for number in 2..=5 {
        assert_historical_interval(
            step_by_number(number),
            reported_job_end,
            reported_job_end,
            "fallback",
        );
    }

    let malformed_completion = captured.workflow_job_for_delivery(malformed_completion_delivery);
    let malformed_completion_at = rfc3339_unix_nanos(malformed_completion_at);
    assert_historical_interval(
        malformed_completion,
        malformed_completion_at,
        malformed_completion_at,
        "fallback",
    );

    let missing = captured.workflow_job_for_delivery(missing_delivery);
    assert_eq!(missing.start_time_unix_nano, missing.end_time_unix_nano);
    assert!(
        (missing_request_started..=missing_request_completed)
            .contains(&missing.start_time_unix_nano),
        "missing timestamps fall back to the exact bounded receipt instant"
    );
    assert_historical_interval(
        missing,
        missing.start_time_unix_nano,
        missing.end_time_unix_nano,
        "fallback",
    );

    let malformed_receipt = captured.workflow_job_for_delivery(malformed_receipt_delivery);
    assert_eq!(
        malformed_receipt.start_time_unix_nano,
        malformed_receipt.end_time_unix_nano
    );
    assert!(
        (malformed_request_started..=malformed_request_completed)
            .contains(&malformed_receipt.start_time_unix_nano),
        "malformed timestamps fall back to the exact bounded receipt instant"
    );
    assert_historical_interval(
        malformed_receipt,
        malformed_receipt.start_time_unix_nano,
        malformed_receipt.end_time_unix_nano,
        "fallback",
    );

    let reversed = captured.workflow_job_for_delivery(reversed_delivery);
    let reversed_completion_at = rfc3339_unix_nanos(reversed_completion_at);
    assert_historical_interval(
        reversed,
        reversed_completion_at,
        reversed_completion_at,
        "fallback",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unsupported_workflow_actions_and_projections_emit_no_historical_trace() {
    let fixture = WebhookTraceFixture::new().await;
    let requests = [
        (
            "550e8400-e29b-41d4-a716-446655440500",
            Some("queued"),
            "queued",
            serde_json::json!({
                "id": 501, "run_id": 601, "run_attempt": 1,
                "completed_at": "2026-08-06T10:05:00Z", "steps": []
            }),
        ),
        (
            "550e8400-e29b-41d4-a716-446655440501",
            Some("in_progress"),
            "in_progress",
            serde_json::json!({
                "id": 502, "run_id": 602, "run_attempt": 1,
                "completed_at": "2026-08-06T10:05:00Z", "steps": []
            }),
        ),
        (
            "550e8400-e29b-41d4-a716-446655440502",
            None,
            "none",
            serde_json::json!({
                "id": 503, "run_id": 603, "run_attempt": 1,
                "completed_at": "2026-08-06T10:05:00Z", "steps": []
            }),
        ),
        (
            "550e8400-e29b-41d4-a716-446655440503",
            Some("fixture_unknown_action"),
            "other",
            serde_json::json!({
                "id": 504, "run_id": 604, "run_attempt": 1,
                "completed_at": "2026-08-06T10:05:00Z", "steps": []
            }),
        ),
        (
            "550e8400-e29b-41d4-a716-446655440504",
            Some("completed"),
            "completed",
            serde_json::json!({
                "id": 0, "run_id": 605, "run_attempt": 1,
                "completed_at": "2026-08-06T10:05:00Z", "steps": []
            }),
        ),
        (
            "550e8400-e29b-41d4-a716-446655440505",
            Some("completed"),
            "completed",
            serde_json::json!({
                "id": 506, "run_id": "malformed", "run_attempt": 1,
                "completed_at": "2026-08-06T10:05:00Z", "steps": []
            }),
        ),
        (
            "550e8400-e29b-41d4-a716-446655440506",
            Some("completed"),
            "completed",
            serde_json::json!({
                "id": 507, "run_id": 607,
                "completed_at": "2026-08-06T10:05:00Z", "steps": []
            }),
        ),
        (
            "550e8400-e29b-41d4-a716-446655440507",
            Some("completed"),
            "completed",
            serde_json::json!({
                "id": 508, "run_id": 608, "run_attempt": 1,
                "completed_at": "2026-08-06T10:05:00Z", "steps": {}
            }),
        ),
        (
            "550e8400-e29b-41d4-a716-446655440508",
            Some("completed"),
            "completed",
            serde_json::json!({
                "id": 509, "run_id": 609, "run_attempt": 1,
                "completed_at": "2026-08-06T10:05:00Z",
                "steps": [{"name": "missing required number"}]
            }),
        ),
        (
            "550e8400-e29b-41d4-a716-446655440509",
            Some("completed"),
            "completed",
            serde_json::json!({
                "id": 510, "run_id": 610, "run_attempt": 1,
                "completed_at": "2026-08-06T10:05:00Z",
                "steps": [{"number": 1}, {"number": 0}, {"number": 2}]
            }),
        ),
        (
            "550e8400-e29b-41d4-a716-446655440510",
            Some("completed"),
            "completed",
            serde_json::json!({
                "id": 511, "run_id": 611, "run_attempt": 1,
                "completed_at": "2026-08-06T10:05:00Z",
                "steps": [{"number": 1}, {"number": -1}, {"number": 2}]
            }),
        ),
    ];

    for (delivery_id, action, normalized_action, workflow_job) in &requests {
        let delivery_id = *delivery_id;
        let normalized_action = *normalized_action;
        let event_series = format!(
            "github_webhook_events_total{{repository=\"owner/webhook-private-repository\",event_type=\"workflow_job\",action=\"{normalized_action}\"}}"
        );
        let body_series =
            "github_webhook_request_body_bytes_count{repository=\"owner/webhook-private-repository\"}";
        let accepted_series =
            "github_webhook_requests_total{repository=\"owner/webhook-private-repository\",result=\"accepted\"}";
        let before = fixture.metrics_text().await;
        let event_count = metric_u64_or_zero(&before, &event_series);
        let body_count = metric_u64_or_zero(&before, body_series);
        let accepted_count = metric_u64_or_zero(&before, accepted_series);

        let body = workflow_job_body(*action, workflow_job.clone());
        let response = fixture
            .webhook(&body, "workflow_job", delivery_id, WEBHOOK_SECRET)
            .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);

        let claim_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM processed_deliveries WHERE delivery_id = ?")
                .bind(delivery_id)
                .fetch_one(&fixture.pool)
                .await
                .expect("delivery claim is countable");
        assert_eq!(claim_count, 1, "durable claim for {delivery_id}");

        let after = fixture.metrics_text().await;
        assert_eq!(
            metric_u64(&after, &event_series),
            event_count + 1,
            "generic event metric delta for {delivery_id}"
        );
        assert_eq!(
            metric_u64(&after, body_series),
            body_count + 1,
            "generic body metric delta for {delivery_id}"
        );
        assert_eq!(
            metric_u64(&after, accepted_series),
            accepted_count + 1,
            "accepted request metric delta for {delivery_id}"
        );

        let captured = fixture.force_flush();
        assert_eq!(
            captured
                .spans
                .iter()
                .filter(|span| is_workflow_task_span(span))
                .count(),
            0,
            "request {delivery_id} emits no historical workflow span"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_workflow_delivery_emits_one_historical_trace() {
    let fixture = WebhookTraceFixture::new().await;
    let delivery_id = "550e8400-e29b-41d4-a716-446655440600";
    let first_step_started_at = "2026-08-06T10:01:00.000000003Z";
    let first_step_completed_at = "2026-08-06T10:02:00.000000004Z";
    let second_step_started_at = "2026-08-06T10:03:00.000000005Z";
    let second_step_completed_at = "2026-08-06T10:04:00.000000006Z";
    let body = workflow_job_body(
        Some("completed"),
        serde_json::json!({
            "id": 701,
            "run_id": 801,
            "run_attempt": 1,
            "conclusion": "success",
            "started_at": "2026-08-06T10:00:00.000000001Z",
            "completed_at": "2026-08-06T10:05:00.000000002Z",
            "steps": [
                {
                    "number": 1,
                    "conclusion": "success",
                    "started_at": first_step_started_at,
                    "completed_at": first_step_completed_at
                },
                {
                    "number": 2,
                    "conclusion": "failure",
                    "started_at": second_step_started_at,
                    "completed_at": second_step_completed_at
                }
            ]
        }),
    );

    for _ in 0..2 {
        let response = fixture
            .webhook(&body, "workflow_job", delivery_id, WEBHOOK_SECRET)
            .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    let claim_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM processed_deliveries WHERE delivery_id = ?")
            .bind(delivery_id)
            .fetch_one(&fixture.pool)
            .await
            .expect("durable delivery claim is countable");
    assert_eq!(claim_count, 1);
    let exposition = fixture.metrics_text().await;
    let captured = fixture.force_flush();

    let job = captured.workflow_job_for_delivery(delivery_id);
    assert_eq!(
        captured
            .children(job)
            .filter(|span| string_attribute(span, "sentry.op") == Some("github.actions.step"))
            .count(),
        2
    );
    let first_step = captured.workflow_step_for_task_run_id(job, "701:1");
    let second_step = captured.workflow_step_for_task_run_id(job, "701:2");
    assert_attribute(first_step, "github.workflow.conclusion", "success");
    assert_attribute(first_step, "cicd.pipeline.task.run.result", "success");
    assert_otlp_status(first_step, OtlpStatusCode::Ok, "");
    assert_historical_interval(
        first_step,
        rfc3339_unix_nanos(first_step_started_at),
        rfc3339_unix_nanos(first_step_completed_at),
        "reported",
    );
    assert_attribute(second_step, "github.workflow.conclusion", "failure");
    assert_attribute(second_step, "cicd.pipeline.task.run.result", "failure");
    assert_otlp_status(second_step, OtlpStatusCode::Error, "workflow_failed");
    assert_historical_interval(
        second_step,
        rfc3339_unix_nanos(second_step_started_at),
        rfc3339_unix_nanos(second_step_completed_at),
        "reported",
    );
    for expected in [
        "github_webhook_requests_total{repository=\"owner/webhook-private-repository\",result=\"accepted\"} 2",
        "github_webhook_events_total{repository=\"owner/webhook-private-repository\",event_type=\"workflow_job\",action=\"completed\"} 1",
        "github_webhook_request_body_bytes_count{repository=\"owner/webhook-private-repository\"} 1",
        "github_webhook_duplicates_total{repository=\"owner/webhook-private-repository\"} 1",
    ] {
        assert_metric_line(&exposition, expected);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn duplicate_over_limit_workflow_job_records_one_rejection() {
    let fixture = WebhookTraceFixture::new_with_workflow_job_max_steps(2).await;
    let delivery_id = "550e8400-e29b-41d4-a716-446655440802";
    let body = workflow_job_body(
        Some("completed"),
        serde_json::json!({
            "id": 9902,
            "run_id": 8802,
            "run_attempt": 1,
            "conclusion": "success",
            "steps": [
                {"number": 1, "conclusion": "success"},
                {"number": 2, "conclusion": "success"},
                {"number": 3, "conclusion": "success"}
            ]
        }),
    );

    for _ in 0..2 {
        let response = fixture
            .webhook(&body, "workflow_job", delivery_id, WEBHOOK_SECRET)
            .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    let claim_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM processed_deliveries WHERE delivery_id = ?")
            .bind(delivery_id)
            .fetch_one(&fixture.pool)
            .await
            .expect("durable delivery claim is countable");
    assert_eq!(claim_count, 1);

    let exposition = fixture.metrics_text().await;
    let captured = fixture.force_flush();
    let stderr = fixture.output.text();
    assert_metric_line(
        &exposition,
        "github_webhook_requests_total{repository=\"owner/webhook-private-repository\",result=\"accepted\"} 2",
    );
    assert_metric_line(
        &exposition,
        "github_webhook_events_total{repository=\"owner/webhook-private-repository\",event_type=\"workflow_job\",action=\"completed\"} 1",
    );
    assert_metric_line(
        &exposition,
        "github_webhook_duplicates_total{repository=\"owner/webhook-private-repository\"} 1",
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_steps_count{repository=\"owner/webhook-private-repository\"} 1",
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_steps_sum{repository=\"owner/webhook-private-repository\"} 3.0",
    );
    assert_metric_line(
        &exposition,
        "github_workflow_job_trace_rejections_total{repository=\"owner/webhook-private-repository\",reason=\"too_many_steps\"} 1",
    );
    assert_eq!(
        captured
            .spans
            .iter()
            .filter(|span| is_workflow_task_span(span))
            .count(),
        0,
        "over-limit duplicate delivery emits no historical workflow spans"
    );
    assert_eq!(
        stderr
            .matches("completed workflow-job trace rejected")
            .count(),
        1,
        "duplicate delivery emits one completed workflow-job rejection warning"
    );
    assert_eq!(
        captured
            .log_records
            .iter()
            .filter(|record| {
                record
                    .body
                    .as_ref()
                    .and_then(string_any_value)
                    .is_some_and(|body| body.contains("completed workflow-job trace rejected"))
            })
            .count(),
        1,
        "duplicate delivery exports one completed workflow-job rejection warning"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collector_outage_is_counted_without_affecting_webhook_or_readiness() {
    let mut fixture = WebhookTraceFixture::new().await;
    fixture.receiver.stop().await;

    let ready_before = fixture
        .request(Method::GET, "/health/ready", None, None, Body::empty())
        .await;
    assert_eq!(ready_before.status(), StatusCode::OK);
    let body = workflow_job_body(
        Some("completed"),
        serde_json::json!({
            "id": 991,
            "run_id": 992,
            "run_attempt": 1,
            "conclusion": "success",
            "started_at": "2026-08-07T12:00:00Z",
            "completed_at": "2026-08-07T12:01:00Z",
            "steps": []
        }),
    );
    let response = fixture
        .webhook(
            &body,
            "workflow_job",
            "550e8400-e29b-41d4-a716-446655440799",
            WEBHOOK_SECRET,
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    tracing::dispatcher::with_default(&fixture.dispatch, || {
        tracing::info!(target: "github_webhook_exporter", "outage-test-log");
    });
    tokio::task::block_in_place(|| fixture.runtime.force_flush().expect("providers flush"));

    let ready_after = fixture
        .request(Method::GET, "/health/ready", None, None, Body::empty())
        .await;
    assert_eq!(ready_after.status(), StatusCode::OK);
    tokio::task::block_in_place(|| {
        fixture
            .runtime
            .force_flush()
            .expect("readiness trace flushes")
    });
    let expected_failures = [
        ("trace", fixture.runtime.failed_trace_exports()),
        ("log", fixture.runtime.failed_log_exports()),
    ];
    let exposition = fixture.metrics_text().await;
    for (signal, failures) in expected_failures {
        assert!(failures > 0, "{signal} export must fail during outage");
        assert_metric_line(
            &exposition,
            &format!(
                "github_telemetry_export_failures_total{{signal=\"{signal}\",reason=\"transport\"}} {failures}"
            ),
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn collector_http_failure_is_classified_without_exposing_response_body() {
    let fixture = WebhookTraceFixture::new().await;
    fixture
        .receiver
        .set_response_status(StatusCode::SERVICE_UNAVAILABLE);

    let body = workflow_job_body(
        Some("completed"),
        serde_json::json!({
            "id": 993,
            "run_id": 994,
            "run_attempt": 1,
            "conclusion": "success",
            "started_at": "2026-08-07T13:00:00Z",
            "completed_at": "2026-08-07T13:01:00Z",
            "steps": []
        }),
    );
    let response = fixture
        .webhook(
            &body,
            "workflow_job",
            "550e8400-e29b-41d4-a716-446655440798",
            WEBHOOK_SECRET,
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    tracing::dispatcher::with_default(&fixture.dispatch, || {
        tracing::info!(target: "github_webhook_exporter", "http-failure-test-log");
    });
    let ready = fixture
        .request(Method::GET, "/health/ready", None, None, Body::empty())
        .await;
    assert_eq!(ready.status(), StatusCode::OK);
    // This fixture intentionally rejects every export. Depending on worker timing, force-flush may
    // return the expected aggregate export failure; the counters below verify completed attempts.
    drop(tokio::task::block_in_place(|| {
        fixture.runtime.force_flush()
    }));

    let expected_failures = [
        ("trace", fixture.runtime.failed_trace_exports()),
        ("log", fixture.runtime.failed_log_exports()),
    ];
    let exposition = fixture.metrics_text().await;
    for (signal, failures) in expected_failures {
        assert!(failures > 0, "{signal} export must receive HTTP 503");
        assert_metric_line(
            &exposition,
            &format!(
                "github_telemetry_export_failures_total{{signal=\"{signal}\",reason=\"http_response\"}} {failures}"
            ),
        );
        assert_metric_line(
            &exposition,
            &format!(
                "github_telemetry_export_failures_total{{signal=\"{signal}\",reason=\"encoding\"}} 0"
            ),
        );
    }
    assert!(!exposition.contains("collector-secret-response-body"));
    assert!(!fixture
        .output
        .text()
        .contains("collector-secret-response-body"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn blocked_collector_does_not_change_completed_workflow_response() {
    const EXPORTER_TIMEOUT_MILLIS: u64 = 10_000;
    const WEBHOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    let fixture = WebhookTraceFixture::new_with_exporter_timeout(EXPORTER_TIMEOUT_MILLIS).await;
    let ready_before = fixture
        .request(Method::GET, "/health/ready", None, None, Body::empty())
        .await;
    assert_eq!(ready_before.status(), StatusCode::OK);
    fixture.flush();
    fixture.receiver.clear_captured_requests();
    assert_eq!(fixture.runtime.failed_trace_exports(), 0);
    assert_eq!(fixture.runtime.failed_log_exports(), 0);

    let collector_endpoint = fixture.receiver.endpoint();
    let collector_address = fixture.receiver.address.to_string();
    let started_requests = fixture.receiver.started_request_count();
    fixture.receiver.set_exports_released(false);

    let delivery_id = "550e8400-e29b-41d4-a716-446655440700";
    let body = workflow_job_body(
        Some("completed"),
        serde_json::json!({
            "id": 901,
            "run_id": 902,
            "run_attempt": 1,
            "workflow_name": "Blocked collector workflow",
            "name": "Blocked collector job",
            "conclusion": "success",
            "started_at": "2026-08-06T14:00:00Z",
            "completed_at": "2026-08-06T14:05:00Z",
            "steps": [{
                "number": 1,
                "name": "Blocked collector step",
                "conclusion": "success",
                "started_at": "2026-08-06T14:01:00Z",
                "completed_at": "2026-08-06T14:02:00Z"
            }]
        }),
    );
    let response = tokio::time::timeout(
        WEBHOOK_TIMEOUT,
        fixture.webhook(&body, "workflow_job", delivery_id, WEBHOOK_SECRET),
    )
    .await
    .expect("webhook must not wait for the blocked ten-second exporter");

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(to_bytes(response.into_body(), 1)
        .await
        .expect("workflow response body is readable")
        .is_empty());

    tokio::time::timeout(
        WEBHOOK_TIMEOUT,
        wait_for_started_requests(&fixture.receiver.state, started_requests + 1),
    )
    .await
    .expect("trace export reaches the blocked collector");
    let (blocked_traces, blocked_logs) = fixture.receiver.captured_requests();
    assert!(!blocked_traces.is_empty(), "a trace export is blocked");
    // The generic completion event is local-only, and this success path emits no other logs.
    // A future remotely eligible webhook log intentionally breaks this zero-export invariant.
    assert!(
        blocked_logs.is_empty(),
        "local-only webhook completion logs must not reach the blocked collector"
    );

    let ready_after = fixture
        .request(Method::GET, "/health/ready", None, None, Body::empty())
        .await;
    assert_eq!(ready_after.status(), StatusCode::OK);
    let exposition = fixture.metrics_text().await;
    for expected in [
        "github_webhook_requests_total{repository=\"owner/webhook-private-repository\",result=\"accepted\"} 1",
        "github_webhook_events_total{repository=\"owner/webhook-private-repository\",event_type=\"workflow_job\",action=\"completed\"} 1",
        "github_webhook_request_body_bytes_count{repository=\"owner/webhook-private-repository\"} 1",
        "github_webhook_duplicates_total{repository=\"unknown\"} 0",
    ] {
        assert_metric_line(&exposition, expected);
    }
    let merge_queue_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM merge_queue_attempts")
        .fetch_one(&fixture.pool)
        .await
        .expect("merge-queue state is countable");
    assert_eq!(merge_queue_rows, 0);
    assert_eq!(fixture.runtime.state(), TelemetryState::Enabled);
    assert_eq!(fixture.runtime.failed_trace_exports(), 0);
    assert_eq!(fixture.runtime.failed_log_exports(), 0);
    assert_eq!(fixture.runtime.dropped_trace_records(), 0);
    assert_eq!(fixture.runtime.dropped_log_records(), 0);

    fixture.receiver.set_exports_released(true);
    let captured = fixture.force_flush();
    let job = captured.workflow_job_for_delivery(delivery_id);
    assert_attribute(job, "cicd.pipeline.task.run.id", "901");
    assert_eq!(fixture.runtime.pending_trace_records(), 0);
    assert_eq!(fixture.runtime.pending_log_records(), 0);
    assert_eq!(fixture.runtime.failed_trace_exports(), 0);
    assert_eq!(fixture.runtime.failed_log_exports(), 0);
    assert_eq!(fixture.runtime.dropped_trace_records(), 0);
    assert_eq!(fixture.runtime.dropped_log_records(), 0);

    let stderr = fixture.finish_without_force_flush().await;
    let stderr_lowercase = stderr.to_ascii_lowercase();
    for detail in [collector_endpoint.as_str(), collector_address.as_str()] {
        assert!(
            !stderr.contains(detail),
            "stderr must not expose collector detail {detail:?}"
        );
    }
    for error_detail in [
        "connection refused",
        "error sending request",
        "failed to export",
    ] {
        assert!(
            !stderr_lowercase.contains(error_detail),
            "stderr must not expose exporter error detail {error_detail:?}"
        );
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
    let output = fixture.output.clone();
    let (captured, _) = fixture.finish();

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
        assert!(!output.text().contains(forbidden));
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
    }
    let exposition = fixture.metrics_text().await;
    let (captured, stderr) = fixture.finish();

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
    assert!(exposition.contains(
        "github_webhook_duplicates_total{repository=\"owner/webhook-private-repository\"} 1"
    ));
    assert!(exposition.contains(
        "github_webhook_processing_failures_total{repository=\"owner/webhook-private-repository\",stage=\"queue_state\"} 1"
    ));
    for forbidden in [
        "queue-raw-reason-must-not-appear",
        "queue-store-detail-must-not-appear",
        WEBHOOK_SECRET,
    ] {
        captured.assert_absent(forbidden);
        captured.assert_logs_absent(forbidden);
        assert!(!stderr.contains(forbidden));
        assert!(!exposition.contains(forbidden));
    }
}

async fn exercise_integrated_queue_failure(fixture: &WebhookTraceFixture, repository_id: i64) {
    sqlx::query(
        "INSERT INTO merge_queue_attempts \
             (repository_id, pull_request_number, enqueued_at, outcome, reason_code) \
         VALUES (?, ?, '2026-08-05T10:02:00.000Z', 'pending', 'none')",
    )
    .bind(repository_id)
    .bind(PRIVACY_QUEUE_FAILURE_PR_NUMBER)
    .execute(&fixture.pool)
    .await
    .expect("active queue attempt for persistence failure is inserted");
    sqlx::query(&format!(
        "CREATE TRIGGER reject_integrated_queue_completion \
         BEFORE UPDATE ON merge_queue_attempts \
         BEGIN SELECT RAISE(ABORT, '{RAW_QUEUE_STORE_DETAIL}'); END"
    ))
    .execute(&fixture.pool)
    .await
    .expect("queue persistence failure trigger is installed");
    let queue_failure_body = serde_json::to_vec(&serde_json::json!({
        "action": "dequeued",
        "reason": RAW_REASON,
        "pull_request": {
            "number": PRIVACY_QUEUE_FAILURE_PR_NUMBER,
            "updated_at": "2026-08-05T10:03:00Z",
            "head": {"sha": PRIVACY_SHA}
        },
        "repository": {"full_name": PRIVACY_REPOSITORY},
        "sender": {"login": ACTOR},
        "command": COMMAND,
        "url": RAW_URL
    }))
    .expect("queue persistence failure body serializes");
    let queue_failure = fixture
        .webhook(
            &queue_failure_body,
            "pull_request",
            PRIVACY_QUEUE_FAILURE_DELIVERY,
            SECRET,
        )
        .await;
    assert_eq!(queue_failure.status(), StatusCode::NO_CONTENT);
    drop(queue_failure);
    sqlx::query("DROP TRIGGER reject_integrated_queue_completion")
        .execute(&fixture.pool)
        .await
        .expect("queue persistence failure trigger is removed");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn integrated_core_trace_privacy() {
    use crate::retention::{run_retention_once, RetentionConfig};

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

    exercise_integrated_queue_failure(&fixture, repository_id).await;
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
    .bind(PRIVACY_RETENTION_PR_NUMBER)
    .execute(&fixture.pool)
    .await
    .expect("expired queue attempt is inserted");
    let retention_config = RetentionConfig::new(
        std::time::Duration::from_secs(60),
        std::time::Duration::from_secs(86_400),
        std::time::Duration::from_secs(90 * 86_400),
    )
    .expect("retention configuration is valid");
    let (_shutdown_sender, shutdown_receiver) = watch::channel(false);
    let delivery_store = DeliveryStore::new(fixture.pool.clone());
    let merge_queue_store = MergeQueueStore::new(fixture.pool.clone());
    async {
        let ambient_request = trace::operation_span(trace::Operation::HttpRequest);
        run_retention_once(
            &delivery_store,
            &merge_queue_store,
            retention_config,
            &shutdown_receiver,
        )
        .instrument(ambient_request)
        .await;
    }
    .with_subscriber(fixture.dispatch.clone())
    .await;
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
    .bind(PRIVACY_RETENTION_PR_NUMBER)
    .fetch_one(&fixture.pool)
    .await
    .expect("retention queue attempt is countable");
    assert_eq!(retained_delivery, 0);
    assert_eq!(retained_attempt, 0);

    let exposition = fixture.metrics_text().await;
    let (captured, stderr, shutdown_outcome) = fixture.shutdown();
    assert_eq!(shutdown_outcome, TelemetryShutdownOutcome::Completed);

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

    let dequeue_update = captured
        .spans
        .iter()
        .find(|span| {
            span.name == "merge_queue.update"
                && string_attribute(span, "github.delivery.id") == Some(PRIVACY_DEQUEUE_DELIVERY)
        })
        .expect("dequeue update span is exported");
    let dequeue_process = captured.parent_named(dequeue_update, "github.webhook.process");
    let dequeue_request = captured.parent_named(dequeue_process, "http.request");
    assert_attribute(dequeue_request, "http.route", "/webhooks/github");
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

    let queue_failure_request =
        captured.webhook_request_for_delivery(PRIVACY_QUEUE_FAILURE_DELIVERY);
    let queue_failure_process =
        captured.child_named(queue_failure_request, "github.webhook.process");
    let queue_failure_update =
        captured.descendant_named(queue_failure_process, "merge_queue.update", "pull_request");
    assert_attribute(queue_failure_update, "ghe.operation.outcome", "failure");
    assert_eq!(queue_failure_update.events.len(), 1);
    let queue_failure_event = &queue_failure_update.events[0];
    assert_eq!(queue_failure_event.name, "operation.failure");
    assert_eq!(queue_failure_event.attributes.len(), 1);
    assert_eq!(queue_failure_event.attributes[0].key, "ghe.failure.reason");
    assert_eq!(
        event_string_attribute(queue_failure_event, "ghe.failure.reason"),
        Some("queue_state")
    );
    assert_eq!(
        queue_failure_update
            .status
            .as_ref()
            .map(|status| status.code),
        Some(opentelemetry_proto::tonic::trace::v1::status::StatusCode::Error as i32)
    );

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
    let mut retention_children = captured
        .children(retention_root)
        .map(|span| {
            (
                span.name.as_str(),
                string_attribute(span, "db.operation.name"),
                string_attribute(span, "ghe.operation.outcome"),
            )
        })
        .collect::<Vec<_>>();
    retention_children.sort_unstable();
    assert_eq!(
        retention_children,
        [
            ("sqlite.query", Some("delivery.prune"), Some("success")),
            ("sqlite.query", Some("merge_queue.prune"), Some("success")),
            (
                "sqlite.query",
                Some("workflow_job_link.prune"),
                Some("success")
            ),
            ("sqlite.query", Some("workflow_run.prune"), Some("success")),
        ]
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
        PRIVACY_QUEUE_FAILURE_DELIVERY,
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
    assert!(captured.has_trace_i64_attribute(
        "github.pull_request.number",
        PRIVACY_QUEUE_FAILURE_PR_NUMBER,
    ));

    captured.assert_approved_attribute_keys();
    for key in SPAN_ONLY_ATTRIBUTE_KEYS {
        assert!(
            !captured.has_log_attribute_key(key),
            "OTLP logs must not contain span-only identifier key {key:?}"
        );
    }
    for (key, value) in [
        ("github.repository.id", repository_id),
        ("github.pull_request.number", PRIVACY_PR_NUMBER),
        (
            "github.pull_request.number",
            PRIVACY_QUEUE_FAILURE_PR_NUMBER,
        ),
    ] {
        assert!(captured.has_trace_i64_attribute(key, value));
        assert!(
            !captured.has_log_i64_attribute(key, value),
            "OTLP logs must not contain span-only integer {key}={value}"
        );
        assert!(
            !captured.has_log_i64_value(value),
            "OTLP log attributes must not contain span-only integer {value}"
        );
    }
    assert!(
        !captured.has_trace_i64_value(PRIVACY_RETENTION_PR_NUMBER),
        "retention pull-request identifiers must not appear in trace attributes"
    );
    assert!(
        !captured.has_log_i64_value(PRIVACY_RETENTION_PR_NUMBER),
        "retention pull-request identifiers must not appear in log attributes"
    );
    assert!(
        !captured.has_trace_string_attribute("github.delivery.id", PRIVACY_RETENTION_DELIVERY,),
        "retention delivery identifiers must not appear in trace attributes"
    );

    assert!(exposition.contains(&format!("repository=\"{PRIVACY_REPOSITORY}\"")));
    captured.assert_logs_absent(PRIVACY_REPOSITORY);
    assert!(!stderr.contains(PRIVACY_REPOSITORY));
    for approved_identifier in [
        PRIVACY_MERGE_GROUP_DELIVERY,
        PRIVACY_ENQUEUE_DELIVERY,
        PRIVACY_DEQUEUE_DELIVERY,
        PRIVACY_AUTH_FAILURE_DELIVERY,
        PRIVACY_PROCESS_FAILURE_DELIVERY,
        PRIVACY_QUEUE_FAILURE_DELIVERY,
        PRIVACY_SHA,
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
    for numeric_identifier in [
        repository_id,
        PRIVACY_PR_NUMBER,
        PRIVACY_QUEUE_FAILURE_PR_NUMBER,
    ] {
        let numeric_identifier = numeric_identifier.to_string();
        assert!(!stderr.contains(&numeric_identifier));
        assert!(!exposition.contains(&numeric_identifier));
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
        RAW_QUEUE_STORE_DETAIL,
        PRIVACY_RETENTION_DELIVERY,
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
    let retention_pr_number_text = PRIVACY_RETENTION_PR_NUMBER.to_string();
    assert!(!stderr.contains(&retention_pr_number_text));
    assert!(!exposition.contains(&retention_pr_number_text));
    assert!(exposition.contains(&format!(
        "github_webhook_duplicates_total{{repository=\"{PRIVACY_REPOSITORY}\"}} 1"
    )));
    assert!(exposition.contains(&format!(
        "github_webhook_processing_failures_total{{repository=\"{PRIVACY_REPOSITORY}\",stage=\"queue_state\"}} 1"
    )));
}

mod retention {
    use std::{sync::Arc, time::Duration as StdDuration};

    use tracing_subscriber::fmt::MakeWriter;

    use super::*;
    use crate::retention::{run_retention_once, RetentionConfig};

    const RETENTION_TRACE_QUEUE_CAPACITY: usize = 128;

    struct RetentionTraceFixture {
        _otlp_guard: OtlpTestGuard,
        receiver: RunningReceiver,
        runtime: super::super::TelemetryRuntime,
        dispatch: Dispatch,
        span_lifecycles: CapturedSpanLifecycles,
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
            let (runtime, subscriber) = build_runtime(
                "github_webhook_exporter=info",
                &config,
                writer,
                Metrics::new(),
            )
            .expect("telemetry runtime initializes");
            let span_lifecycles = CapturedSpanLifecycles::default();
            let dispatch = Dispatch::new(subscriber.with(span_lifecycles.clone()));
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
                span_lifecycles,
                pool,
                delivery_store,
                merge_queue_store,
                _directory: directory,
            }
        }

        async fn run_pass(&self, config: RetentionConfig, shutdown: &watch::Receiver<bool>) {
            async {
                let ambient_request = trace::operation_span(trace::Operation::HttpRequest);
                run_retention_once(
                    &self.delivery_store,
                    &self.merge_queue_store,
                    config,
                    shutdown,
                )
                .instrument(ambient_request)
                .await;
            }
            .with_subscriber(self.dispatch.clone())
            .await;
        }

        fn finish(self) -> CapturedSpans {
            let Self {
                _otlp_guard,
                receiver,
                runtime,
                dispatch,
                span_lifecycles,
                pool,
                delivery_store,
                merge_queue_store,
                _directory,
            } = self;
            drop(delivery_store);
            drop(merge_queue_store);
            drop(pool);
            drop(dispatch);
            flush_after_span_closure(&runtime, &span_lifecycles);
            let (traces, logs) = receiver.captured_requests();
            drop(runtime);
            drop(receiver);
            drop(_directory);
            drop(_otlp_guard);
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

    fn standard_config() -> RetentionConfig {
        RetentionConfig::new(
            StdDuration::from_secs(60),
            StdDuration::from_secs(86_400),
            StdDuration::from_secs(90 * 86_400),
        )
        .expect("retention configuration is valid")
    }

    fn invalid_cutoff_config() -> RetentionConfig {
        RetentionConfig::new(
            StdDuration::from_secs(60),
            StdDuration::from_secs(100_000_000 * 86_400),
            StdDuration::from_secs(100_000_000 * 86_400),
        )
        .expect("retention configuration is valid")
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
        let (_shutdown_sender, shutdown_receiver) = watch::channel(false);
        fixture
            .run_pass(standard_config(), &shutdown_receiver)
            .await;
        assert_eq!(delivery_count(&fixture.pool).await, 0);
        assert_eq!(queue_attempt_count(&fixture.pool).await, 0);
        let captured = fixture.finish();

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
        fixture
            .run_pass(standard_config(), &shutdown_receiver)
            .await;
        assert_eq!(delivery_count(&fixture.pool).await, 0);
        assert_eq!(queue_attempt_count(&fixture.pool).await, 1);
        let captured = fixture.finish();

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
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn invalid_cutoff_exports_failure_root_without_cutoff_attributes() {
        let fixture = RetentionTraceFixture::new().await;
        let (_shutdown_sender, shutdown_receiver) = watch::channel(false);
        fixture
            .run_pass(invalid_cutoff_config(), &shutdown_receiver)
            .await;
        let captured = fixture.finish();

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
        let (_shutdown_sender, shutdown_receiver) = watch::channel(false);
        fixture
            .run_pass(standard_config(), &shutdown_receiver)
            .await;
        assert_eq!(queue_attempt_count(&fixture.pool).await, 0);
        let captured = fixture.finish();

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
        response_status: AtomicU16::new(StatusCode::OK.as_u16()),
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
    let metrics = Metrics::new();
    let (runtime, subscriber) = build_runtime(
        "github_webhook_exporter=info",
        &config,
        io::sink,
        metrics.clone(),
    )
    .expect("telemetry runtime initializes");
    let dispatch = Dispatch::new(subscriber);

    emit_records(&dispatch, 0..1);
    wait_for_started_requests(&state, 2).await;
    emit_records(&dispatch, 1..SATURATION_RECORDS + 1);

    let expected_drops = (SATURATION_RECORDS - QUEUE_CAPACITY) as u64;
    assert_eq!(runtime.state(), TelemetryState::Enabled);
    assert_eq!(runtime.pending_trace_records(), QUEUE_CAPACITY);
    assert_eq!(runtime.pending_log_records(), QUEUE_CAPACITY);
    assert_eq!(runtime.dropped_trace_records(), expected_drops);
    assert_eq!(runtime.dropped_log_records(), expected_drops);
    let exposition = metrics.encode().expect("metrics encode");
    for signal in ["trace", "log"] {
        let sample = format!(
            "github_telemetry_dropped_records_total{{signal=\"{signal}\",reason=\"queue_full\"}} {expected_drops}"
        );
        assert_metric_line(&exposition, &sample);
    }
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
    assert!(
        !format!("{:?}", captures.logs).contains("telemetry pipeline diagnostic"),
        "direct diagnostics must not recursively enter OTLP logs"
    );

    receiver.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn terminal_workflow_run_exports_a_pipeline_trace_linking_every_job_trace() {
    let fixture = WebhookTraceFixture::new().await;
    let requested_delivery_id = "550e8400-e29b-41d4-a716-446655440400";
    let success_job_delivery_id = "550e8400-e29b-41d4-a716-446655440401";
    let failed_job_delivery_id = "550e8400-e29b-41d4-a716-446655440402";
    let completed_delivery_id = "550e8400-e29b-41d4-a716-446655440403";
    let success_started_at = "2026-08-06T10:00:00.000000001Z";
    let success_completed_at = "2026-08-06T10:05:00.000000002Z";
    let failed_started_at = "2026-08-06T10:04:00.000000003Z";
    let failed_completed_at = "2026-08-06T10:09:00.000000004Z";

    let workflow_run_body = |action: &str| {
        serde_json::to_vec(&serde_json::json!({
            "action": action,
            "workflow_run": {
                "id": 31,
                "run_attempt": 2,
                "name": "Build\nWorkflow",
                "event": "merge_group",
                "head_branch": "gh-readonly-queue/main/pr-7-deadbeef",
                "head_sha": WEBHOOK_SHA_40,
                "pull_requests": [{
                    "number": 7,
                    "head": {"ref": "feature/source"},
                    "base": {"ref": "main"}
                }]
            },
            "repository": {"full_name": WEBHOOK_REPOSITORY}
        }))
        .expect("workflow-run payload serializes")
    };
    let workflow_job_body =
        |job_id: i64, name: &str, conclusion: &str, started_at: &str, completed_at: &str| {
            serde_json::to_vec(&serde_json::json!({
                "action": "completed",
                "workflow_job": {
                    "id": job_id,
                    "run_id": 31,
                    "run_attempt": 2,
                    "workflow_name": "Build\nWorkflow",
                    "name": name,
                    "conclusion": conclusion,
                    "head_sha": WEBHOOK_SHA_40,
                    "started_at": started_at,
                    "completed_at": completed_at,
                    "steps": [{
                        "number": 1,
                        "name": "Only Step",
                        "conclusion": conclusion,
                        "started_at": started_at,
                        "completed_at": completed_at
                    }]
                },
                "repository": {"full_name": WEBHOOK_REPOSITORY}
            }))
            .expect("workflow-job payload serializes")
        };

    for (body, event, delivery_id) in [
        (
            workflow_run_body("requested"),
            "workflow_run",
            requested_delivery_id,
        ),
        (
            workflow_job_body(
                41,
                "Linux\tJob",
                "success",
                success_started_at,
                success_completed_at,
            ),
            "workflow_job",
            success_job_delivery_id,
        ),
        (
            workflow_job_body(42, "\n", "failure", failed_started_at, failed_completed_at),
            "workflow_job",
            failed_job_delivery_id,
        ),
        (
            workflow_run_body("completed"),
            "workflow_run",
            completed_delivery_id,
        ),
    ] {
        let response = fixture
            .webhook(&body, event, delivery_id, WEBHOOK_SECRET)
            .await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        drop(response);
    }

    let captured = fixture.force_flush();
    let request = captured.webhook_request_for_delivery(completed_delivery_id);
    let pipeline = captured.one_named("BuildWorkflow");
    assert!(
        pipeline.parent_span_id.is_empty(),
        "the pipeline summary is an independent root"
    );
    assert_ne!(pipeline.trace_id, request.trace_id);
    assert_eq!(pipeline.kind, OtlpSpanKind::Internal as i32);
    assert_otlp_status(pipeline, OtlpStatusCode::Error, "workflow_failed");
    assert_attribute(pipeline, "sentry.op", "github.actions.pipeline");
    assert_attribute(pipeline, "sentry.description", "BuildWorkflow");
    assert_attribute(pipeline, "cicd.pipeline.name", "BuildWorkflow");
    assert_attribute(pipeline, "cicd.pipeline.run.id", "31");
    assert_attribute(
        pipeline,
        "cicd.pipeline.run.url.full",
        &format!("https://github.com/{WEBHOOK_REPOSITORY}/actions/runs/31"),
    );
    assert_attribute(pipeline, "github.workflow.run.attempt", "2");
    assert_attribute(pipeline, "github.workflow.conclusion", "failure");
    assert_attribute(pipeline, "error.type", "GitHubActionsTaskFailure");
    assert_attribute(pipeline, "github.workflow.event", "merge_group");
    assert_attribute(
        pipeline,
        "github.workflow.source_branch",
        "gh-readonly-queue/main/pr-7-deadbeef",
    );
    assert_attribute(pipeline, "github.workflow.target_branch", "main");
    assert_attribute(pipeline, "github.delivery.id", completed_delivery_id);
    assert_attribute(
        pipeline,
        "vcs.repository.name",
        "webhook-private-repository",
    );
    assert_attribute(
        pipeline,
        "vcs.ref.head.revision",
        &WEBHOOK_SHA_40.to_ascii_lowercase(),
    );
    assert_attribute(pipeline, "timing_source", "reported");
    assert_i64_array_attribute(pipeline, "github.pull_request.number", &[7]);
    assert_eq!(
        pipeline.start_time_unix_nano,
        rfc3339_unix_nanos(success_started_at)
    );
    assert_eq!(
        pipeline.end_time_unix_nano,
        rfc3339_unix_nanos(failed_completed_at)
    );
    assert!(pipeline.links.is_empty());

    let mut summaries = captured.children(pipeline).collect::<Vec<_>>();
    summaries.sort_unstable_by_key(|span| span.start_time_unix_nano);
    assert_eq!(summaries.len(), 2);
    assert!(summaries
        .iter()
        .all(|span| span.trace_id == pipeline.trace_id
            && span.kind == OtlpSpanKind::Internal as i32
            && string_attribute(span, "sentry.op") == Some("github.actions.pipeline.task")));

    // Summary children deliberately reuse the job waterfall label, so job roots are selected by
    // their distinct operation rather than by name.
    let job_roots = ["BuildWorkflow / LinuxJob", "BuildWorkflow / job"].map(|name| {
        let matches = captured
            .spans
            .iter()
            .filter(|span| {
                span.name == name
                    && string_attribute(span, "sentry.op") == Some("github.actions.job")
            })
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1, "one job root for {name}");
        matches[0]
    });
    for (summary, job_root) in summaries.iter().zip(job_roots) {
        assert_ne!(
            summary.trace_id, job_root.trace_id,
            "job traces stay independent of the pipeline trace"
        );
        assert_eq!(summary.links.len(), 1, "one link per summarized job");
        let link = &summary.links[0];
        assert_eq!(link.trace_id, job_root.trace_id);
        assert_eq!(link.span_id, job_root.span_id);
        assert_eq!(summary.name, job_root.name);
        assert_eq!(summary.start_time_unix_nano, job_root.start_time_unix_nano);
        assert_eq!(summary.end_time_unix_nano, job_root.end_time_unix_nano);
        assert_eq!(
            captured.children(summary).count(),
            0,
            "steps stay on the job trace"
        );
    }

    assert_attribute(summaries[0], "cicd.pipeline.task.name", "LinuxJob");
    assert_attribute(summaries[0], "cicd.pipeline.task.run.id", "41");
    assert_attribute(summaries[0], "cicd.pipeline.task.run.result", "success");
    assert_attribute(
        summaries[0],
        "cicd.pipeline.task.run.url.full",
        &format!("https://github.com/{WEBHOOK_REPOSITORY}/actions/runs/31/job/41"),
    );
    assert_otlp_status(summaries[0], OtlpStatusCode::Ok, "");
    assert_attribute(summaries[1], "cicd.pipeline.task.name", "job");
    assert_attribute(summaries[1], "cicd.pipeline.task.run.id", "42");
    assert_attribute(summaries[1], "cicd.pipeline.task.run.result", "failure");
    assert_attribute(summaries[1], "error.type", "GitHubActionsTaskFailure");
    assert_otlp_status(summaries[1], OtlpStatusCode::Error, "workflow_failed");
    assert!(
        summaries.iter().all(|span| span.events.is_empty()),
        "synthetic workflow errors stay on the job traces"
    );

    let exposition = fixture.metrics_text().await;
    assert_metric_line(
        &exposition,
        "github_workflow_job_trace_rejections_total{repository=\"unknown\",reason=\"too_many_jobs\"} 0",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_without_emitted_job_traces_exports_no_pipeline_trace() {
    let fixture = WebhookTraceFixture::new().await;
    let body = serde_json::to_vec(&serde_json::json!({
        "action": "completed",
        "workflow_run": {
            "id": 31,
            "run_attempt": 1,
            "name": "Build Workflow",
            "event": "push",
            "head_branch": "main",
            "pull_requests": []
        },
        "repository": {"full_name": WEBHOOK_REPOSITORY}
    }))
    .expect("workflow-run payload serializes");

    let response = fixture
        .webhook(
            &body,
            "workflow_run",
            "550e8400-e29b-41d4-a716-446655440410",
            WEBHOOK_SECRET,
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    drop(response);

    let captured = fixture.force_flush();
    assert!(
        !captured
            .spans
            .iter()
            .any(|span| string_attribute(span, "sentry.op") == Some("github.actions.pipeline")),
        "a run with nothing to summarize emits no pipeline trace"
    );
}
