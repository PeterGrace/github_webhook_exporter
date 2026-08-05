use std::sync::{Arc, Mutex};

use axum::{
    body::Bytes,
    extract::State,
    http::{header::CONTENT_TYPE, HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
    Router,
};
use github_webhook_exporter::{
    config::RuntimeConfig,
    telemetry::{self, TelemetryState},
};
use opentelemetry_proto::tonic::collector::{
    logs::v1::ExportLogsServiceRequest, trace::v1::ExportTraceServiceRequest,
};
use prost::Message;
use tokio::net::TcpListener;

#[derive(Default)]
struct Captures {
    traces: Vec<ExportTraceServiceRequest>,
    logs: Vec<ExportLogsServiceRequest>,
}

type SharedCaptures = Arc<Mutex<Captures>>;

async fn receive_traces(
    State(captures): State<SharedCaptures>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
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
    let request = ExportTraceServiceRequest::decode(body).expect("trace request is protobuf");
    captures
        .lock()
        .expect("capture lock is available")
        .traces
        .push(request);
    (
        StatusCode::OK,
        [(CONTENT_TYPE, "application/x-protobuf")],
        Bytes::new(),
    )
}

async fn receive_logs(
    State(captures): State<SharedCaptures>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
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
    let request = ExportLogsServiceRequest::decode(body).expect("log request is protobuf");
    captures
        .lock()
        .expect("capture lock is available")
        .logs
        .push(request);
    (
        StatusCode::OK,
        [(CONTENT_TYPE, "application/x-protobuf")],
        Bytes::new(),
    )
}

fn set_test_environment(endpoint: &str) {
    for (name, value) in [
        ("GHE_DATABASE_PATH", "/tmp/unused-otlp-test.db"),
        (
            "GHE_MASTER_KEY",
            "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc=",
        ),
        ("GHE_ADMIN_TOKEN", "otlp-test-admin-token"),
        ("OTEL_EXPORTER_OTLP_ENDPOINT", endpoint),
        ("OTEL_EXPORTER_OTLP_HEADERS", "x-test-token=private-value"),
        ("OTEL_EXPORTER_OTLP_TIMEOUT", "2000"),
        ("OTEL_SERVICE_NAME", "github-webhook-exporter-test"),
        (
            "OTEL_RESOURCE_ATTRIBUTES",
            "k8s.pod.name=exporter-0,k8s.namespace.name=tests,forbidden=value",
        ),
        ("GHE_OTEL_QUEUE_CAPACITY", "16"),
        ("GHE_OTEL_BATCH_SIZE", "2"),
    ] {
        std::env::set_var(name, value);
    }
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synthetic_spans_and_logs_export_as_bounded_otlp_http_protobuf() {
    let captures = SharedCaptures::default();
    let app = Router::new()
        .route("/v1/traces", post(receive_traces))
        .route("/v1/logs", post(receive_logs))
        .with_state(Arc::clone(&captures));
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
    set_test_environment(&format!("http://{address}"));
    let config = RuntimeConfig::from_env().expect("runtime configuration is valid");
    let runtime = telemetry::init(config.rust_log(), config.telemetry())
        .expect("telemetry runtime initializes");

    for sequence in 0..3_u64 {
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
    let pending_traces_before_flush = runtime.pending_trace_records();
    let pending_logs_before_flush = runtime.pending_log_records();
    runtime.force_flush().expect("providers flush");

    assert_eq!(runtime.state(), TelemetryState::Enabled);
    assert_eq!(runtime.dropped_trace_records(), 0);
    assert_eq!(runtime.dropped_log_records(), 0);
    assert_eq!(runtime.failed_trace_exports(), 0);
    assert_eq!(runtime.failed_log_exports(), 0);
    let captures = captures.lock().expect("capture lock is available");
    let trace_batches = trace_batch_sizes(&captures.traces);
    let log_batches = log_batch_sizes(&captures.logs);
    assert_eq!(
        trace_batches.iter().sum::<usize>(),
        3,
        "pending before flush: traces={pending_traces_before_flush}, logs={pending_logs_before_flush}; captured requests: traces={}, logs={}",
        captures.traces.len(),
        captures.logs.len()
    );
    assert_eq!(log_batches.iter().sum::<usize>(), 3);
    assert!(trace_batches.iter().all(|size| *size <= 2));
    assert!(log_batches.iter().all(|size| *size <= 2));
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
