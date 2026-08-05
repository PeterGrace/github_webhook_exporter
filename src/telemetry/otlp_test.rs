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
    body::Bytes,
    extract::State,
    http::{header::CONTENT_TYPE, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Router,
};
use opentelemetry_proto::tonic::collector::{
    logs::v1::ExportLogsServiceRequest, trace::v1::ExportTraceServiceRequest,
};
use prost::Message;
use tokio::{
    net::TcpListener,
    sync::{watch, Notify},
};
use tracing::Dispatch;

use crate::config::TelemetryConfig;

use super::{build_runtime, TelemetryState};

const QUEUE_CAPACITY: usize = 4;
const SATURATION_RECORDS: usize = 10;

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
