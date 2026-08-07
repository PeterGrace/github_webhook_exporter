# OTLP Failure and Dropped-Record Diagnostics Design

## Goal

Expose bounded Prometheus counters and privacy-preserving, rate-limited stderr diagnostics for OTLP
trace/log export failures and rejected records without coupling collector health to webhook responses,
readiness, or producer latency.

## Scope and constraints

This change completes issue #35 and builds on the application-owned admission boundaries from #33.
It covers trace and log signals independently. OTLP metrics export, retries, persistence of rejected
records, dashboards, alerts, and final process-level provider shutdown ordering remain out of scope.

All diagnostic dimensions are closed Rust enums. Signals are `trace` and `log`. Export failure
reasons are `transport`, `timeout`, `http_response`, `encoding`, `shutdown`, `internal`, and `other`.
Drop reasons are `queue_full` and `pipeline_closed`. Malformed successful OTLP response protobufs
are classified as `encoding` failures.

Diagnostics must never include endpoint or header values, response bodies, collector text, transport
error text, payload data, span identifiers, repository/workflow identifiers, signatures, secrets, or
other source strings. Telemetry failures remain best effort and cannot affect application results.

## Architecture

### Metrics ownership

`Metrics` gains two pre-seeded labelled counter families:

- `github_telemetry_export_failures_total{signal,reason}`
- `github_telemetry_dropped_records_total{signal,reason}`

Every valid signal/reason series is created at startup, so exposition is stable before the first
failure. The labels use private closed enums implementing `EncodeLabelValue`; no caller can create an
unbounded label.

Startup constructs one `Metrics` value before telemetry initialization. A clone is passed to the
telemetry diagnostics observer and the same value is installed into `AppState`. This ensures the
pipeline hooks and `/metrics` share the exact same counters without polling runtime atomics.

### Diagnostics observer

A focused `telemetry::diagnostics` module owns a cloneable, thread-safe observer. Queue/export hooks
call one of two methods with closed signal/reason enums. Each call increments its Prometheus counter
before attempting local reporting.

The observer stores fixed-size per-category limiter state rather than a dynamic map. Each category
uses atomic monotonic-millisecond deadlines and an atomic suppressed count. The first event reports
immediately; events before the next one-minute deadline only increment suppression; the next
permitted report includes the accumulated suppressed count. Compare-and-exchange guarantees at most
one report per category per interval under concurrency.

Production time comes from `Instant::elapsed`; tests inject a controlled monotonic clock. Production
output goes through a dedicated stderr sink. Tests inject a capture sink. Sink output is assembled
only from fixed literals, bounded enum values, and integer counts. It does not use `tracing`, the
OpenTelemetry log API, or the normal subscriber, preventing recursive OTLP log generation.

A sink failure is ignored after the metric update. Diagnostic reporting never changes queue/export
results.

### Queue admission and closure

The existing trace/log admission processors receive the observer and their fixed signal. Admission
returns a typed outcome:

- capacity exhausted: record one `queue_full` drop;
- processor already closed: record one `pipeline_closed` drop;
- accepted: delegate to the SDK batch processor.

Processor shutdown atomically closes admission before delegating to the SDK. Producer entry points
remain lock-free and never wait for collector I/O. Existing exact pending-count and capacity
invariants remain unchanged.

### HTTP and exporter classification

The OpenTelemetry OTLP exporters retain responsibility for request serialization and protocol
handling. Each enabled signal receives an observing `opentelemetry_http::HttpClient` around a
blocking Reqwest client on the SDK exporter worker thread. The wrapper:

- maps Reqwest timeout errors to `timeout`;
- maps other request/connection failures to `transport`;
- maps non-success HTTP statuses to `http_response` without reading or reporting response text;
- decodes successful trace/log response bodies using the matching generated OTLP response type and
  maps malformed protobuf to `encoding`.

The wrapper returns only redacted static errors to the outer exporter. Export wrappers map public
`OTelSdkError` variants that occur outside an already-classified HTTP attempt to `shutdown`,
`timeout`, `internal`, or `other`. They do not inspect `InternalFailure` strings. Per-export attempt
state prevents one failed request from being counted by both the HTTP client and exporter wrapper.

## Data flow

1. A span or log reaches its application-owned processor.
2. Closed/full admission records one exact drop counter and may emit one bounded diagnostic; the
   record is rejected immediately.
3. Accepted records enter the SDK batch processor and are released from application occupancy when
   an export batch starts.
4. The observing HTTP client classifies request/response failures at the point where structured
   information is still available.
5. The observer increments the exact signal/reason counter and independently applies stderr rate
   limiting.
6. Export errors return only to the SDK worker. No pipeline state enters Axum state decisions,
   readiness, or webhook response construction.

## Error and privacy behavior

Telemetry observation itself is infallible from application callers' perspective. Counter updates
are in-memory atomics. Limiter contention suppresses duplicate output rather than blocking request
processing. Direct diagnostic lines contain only `signal`, `kind`, `reason`, and numeric
`suppressed` fields.

Collector error bodies are never decoded into diagnostics. Raw HTTP status values are not metric
labels. Reqwest and OpenTelemetry error display/debug values are never emitted or used as labels.
Malformed response bodies are discarded after bounded `encoding` classification.

## Testing

Tests follow red-green-refactor cycles and cover:

- complete, zero-valued bounded metric series and exact counter increments;
- exact trace/log `queue_full` and `pipeline_closed` drops with tiny queues;
- concurrent limiter behavior, independent categories, controlled one-minute advancement, and
  suppression accounting;
- direct sink output and proof that diagnostics create no OTLP log records or recursive failures;
- in-process collectors for connection refusal, timeout/slow response, non-success status,
  malformed response, repeated failure, and recovery;
- fixed-value-only stderr and Prometheus output with forbidden endpoint, header, body, transport,
  payload, signature, secret, repository/workflow, and span identifier sentinels absent;
- authenticated webhooks retaining `204`, rejected requests retaining existing statuses, and
  readiness remaining healthy during collector outage and saturation;
- non-blocking enqueue under a blocked exporter.

The final gate is `just fmt`, `cargo build`, `cargo clippy --all-targets -- -D warnings`, `just test`,
and `cargo doc --no-deps`.
