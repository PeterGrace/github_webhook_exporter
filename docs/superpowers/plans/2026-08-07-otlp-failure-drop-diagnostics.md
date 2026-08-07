# OTLP Failure and Dropped-Record Diagnostics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Expose exact bounded Prometheus counters and non-recursive, rate-limited stderr diagnostics for OTLP trace/log failures and dropped records without affecting webhook or readiness behavior.

**Architecture:** A shared `Metrics` instance supplies closed-label telemetry counters to a focused diagnostics observer. Existing queue/export adapters call that observer, while an observing OTLP HTTP client classifies failures before the SDK erases structured transport/response information. Fixed-size atomic limiters produce at most one direct stderr report per signal/reason/minute.

**Tech Stack:** Rust 2021, OpenTelemetry/OTLP 0.32, `opentelemetry-http`, Reqwest blocking client, Prost, `prometheus-client`, Axum, Tokio.

## Global Constraints

- Signals are exactly `trace` and `log`.
- Export failure reasons are exactly `transport`, `timeout`, `http_response`, `encoding`, `shutdown`, `internal`, and `other`.
- Drop reasons are exactly `queue_full` and `pipeline_closed`.
- Malformed successful OTLP response protobufs are `encoding` failures.
- Each category emits at most one direct stderr diagnostic per monotonic minute; the next permitted report includes its suppressed count.
- Diagnostics never use `tracing`, OpenTelemetry logs, or collector/source text.
- Endpoint/header values, response bodies, transport strings, payloads, identifiers, signatures, and secrets never enter counters or diagnostics.
- Queue producer entry points remain lock-free and never wait for collector I/O.
- Collector state never affects readiness or HTTP response decisions.

## File structure

- `src/metrics.rs`: closed telemetry label vocabularies, counter families, updates, and exposition.
- `src/telemetry/diagnostics.rs`: observer, fixed-size atomic limiter, clock, and direct sink.
- `src/telemetry/http_client.rs`: signal-aware OTLP HTTP classification and response validation.
- `src/telemetry/queue.rs`: admission/drop hooks and export/shutdown failure hooks.
- `src/telemetry.rs`: observer/client construction and runtime wiring.
- `src/app.rs`, `src/main.rs`: share one metrics registry between telemetry and HTTP state.
- `src/telemetry/otlp_test.rs`: real collector, saturation, recursion, privacy, response, and readiness coverage.
- `Cargo.toml`, `Cargo.lock`: direct HTTP/protobuf dependencies required by production classification.
- `docs/operations.md`: metric names, labels, interpretation, and outage behavior.
- `changelog/2026-08-07T16-48-23Z-otlp-failure-drop-diagnostics.md`: implementation record.

---

### Task 1: Bounded telemetry Prometheus families and shared startup ownership

**Files:**
- Modify: `src/metrics.rs`
- Modify: `src/app.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Produces: `TelemetrySignal`, `TelemetryExportFailureReason`, `TelemetryDropReason`.
- Produces: `Metrics::record_telemetry_export_failure(signal, reason)` and `Metrics::record_telemetry_drop(signal, reason)`.
- Produces: `AppState::with_metrics(metrics: Metrics) -> Self`.

- [ ] **Step 1: Write failing metric and ownership tests**

In `src/metrics.rs`, add tests that mutate one fixed combination and assert literal exposition. The
production break caught is a missing family, wrong metric name, wrong label text, or a counter update
that lands in the wrong series.

```rust
#[test]
fn telemetry_diagnostic_families_are_complete_and_exact() {
    let metrics = Metrics::new();
    metrics.record_telemetry_export_failure(
        TelemetrySignal::Trace,
        TelemetryExportFailureReason::Timeout,
    );
    metrics.record_telemetry_drop(TelemetrySignal::Log, TelemetryDropReason::QueueFull);

    let exposition = metrics.encode().expect("metrics encode");
    assert!(exposition.contains(
        "github_telemetry_export_failures_total{signal=\"trace\",reason=\"timeout\"} 1"
    ));
    assert!(exposition.contains(
        "github_telemetry_dropped_records_total{signal=\"log\",reason=\"queue_full\"} 1"
    ));
    assert!(exposition.contains(
        "github_telemetry_export_failures_total{signal=\"log\",reason=\"other\"} 0"
    ));
    assert!(exposition.contains(
        "github_telemetry_dropped_records_total{signal=\"trace\",reason=\"pipeline_closed\"} 0"
    ));
}
```

In `src/app.rs`, add a test that installs a pre-mutated `Metrics`, requests `/metrics`, and asserts
that exact mutation is present. This catches accidentally retaining the constructor-created registry.

- [ ] **Step 2: Run tests and verify RED**

Run:

```bash
cargo test metrics::tests::telemetry_diagnostic_families_are_complete_and_exact
cargo test app::tests::installed_metrics_are_served
```

Expected: compilation fails because the enums, methods, and `with_metrics` do not exist.

- [ ] **Step 3: Implement closed labels and counter families**

Add closed enums and literal encoders in `src/metrics.rs`:

```rust
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum TelemetrySignal { Trace, Log }

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum TelemetryExportFailureReason {
    Transport, Timeout, HttpResponse, Encoding, Shutdown, Internal, Other,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum TelemetryDropReason { QueueFull, PipelineClosed }
```

Implement `as_str` and `EncodeLabelValue` with exhaustive matches. Add label sets
`TelemetryExportFailureLabels { signal, reason }` and `TelemetryDropLabels { signal, reason }`.
Create and register `CounterFamily` values under `telemetry_export_failures` and
`telemetry_dropped_records`. Seed all 14 failure combinations and all four drop combinations by
iterating fixed enum arrays. Add crate-private recording methods that call `get_or_create(...).inc()`.

Add this builder to `AppState`:

```rust
pub(crate) fn with_metrics(mut self, metrics: Metrics) -> Self {
    self.metrics = metrics;
    self
}
```

In `main`, construct `let metrics = Metrics::new();` before telemetry initialization, retain a clone
for Task 2, and install the same metrics into `AppState` with `.with_metrics(metrics)`.

- [ ] **Step 4: Verify GREEN**

Run:

```bash
cargo test metrics::tests
cargo test app::tests
```

Expected: PASS with exact bounded series and shared-registry behavior.

- [ ] **Step 5: Commit**

```bash
git add src/metrics.rs src/app.rs src/main.rs
git commit -m "feat: add bounded telemetry diagnostic metrics"
```

---

### Task 2: Atomic one-minute direct diagnostic observer

**Files:**
- Create: `src/telemetry/diagnostics.rs`
- Modify: `src/telemetry.rs`

**Interfaces:**
- Consumes: `Metrics` and the three bounded metric enums from Task 1.
- Produces: cloneable `DiagnosticsObserver` with `export_failure` and `drop_record`.
- Produces: test construction with controlled `Clock` and captured `DiagnosticSink`.

- [ ] **Step 1: Write failing observer tests**

Add unit tests in the new module. Use a test clock backed by `AtomicU64` milliseconds and a capture
sink backed by `Mutex<Vec<String>>`. Assert independently hand-written lines and metric values.
The production breaks caught are missing metrics, wall-clock use, shared-category suppression,
incorrect interval boundaries, lost suppressed counts, or recursive `tracing` output.

```rust
#[test]
fn one_report_per_category_per_minute_includes_suppressed_count() {
    let metrics = Metrics::new();
    let clock = Arc::new(TestClock::default());
    let sink = Arc::new(CaptureSink::default());
    let observer = DiagnosticsObserver::with_dependencies(
        metrics.clone(),
        clock.clone(),
        sink.clone(),
    );

    observer.export_failure(TelemetrySignal::Trace, TelemetryExportFailureReason::Timeout);
    observer.export_failure(TelemetrySignal::Trace, TelemetryExportFailureReason::Timeout);
    observer.export_failure(TelemetrySignal::Trace, TelemetryExportFailureReason::Timeout);
    assert_eq!(sink.lines(), vec![
        "telemetry pipeline diagnostic kind=failure signal=trace reason=timeout suppressed=0\n"
    ]);

    clock.advance(Duration::from_secs(60));
    observer.export_failure(TelemetrySignal::Trace, TelemetryExportFailureReason::Timeout);
    assert_eq!(sink.lines()[1],
        "telemetry pipeline diagnostic kind=failure signal=trace reason=timeout suppressed=2\n"
    );
    assert!(metrics.encode().expect("metrics encode").contains(
        "github_telemetry_export_failures_total{signal=\"trace\",reason=\"timeout\"} 4"
    ));
}
```

Add tests for distinct signal/reason categories, drop categories, 32 concurrent callers producing
one line and suppression `31`, and sink failure leaving the metric count exact.

- [ ] **Step 2: Run tests and verify RED**

Run: `cargo test telemetry::diagnostics::tests`

Expected: compilation fails because `diagnostics` and `DiagnosticsObserver` do not exist.

- [ ] **Step 3: Implement the minimal observer**

Create crate-private `Clock` and `DiagnosticSink` traits requiring `Debug + Send + Sync`. Production
implementations use `Instant::elapsed()` and a single `writeln!(io::stderr().lock(), ...)`. Keep
failure and drop limiters in fixed arrays created with `std::array::from_fn`; derive indices from
exhaustive enum matches, never a dynamic map.

Each limiter stores:

```rust
struct CategoryLimiter {
    next_report_millis: AtomicU64,
    suppressed: AtomicU64,
}
```

On every event, increment `Metrics` first. Compare `now_millis` with `next_report_millis`; one
successful compare-exchange moves the deadline by exactly 60,000 ms and swaps suppression to zero.
A caller that cannot claim the interval increments suppression and returns. Build lines with one
`format!` from fixed enum strings and integer counts only; ignore sink errors.

Expose production construction:

```rust
impl DiagnosticsObserver {
    pub(super) fn new(metrics: Metrics) -> Self;
    pub(super) fn export_failure(
        &self,
        signal: TelemetrySignal,
        reason: TelemetryExportFailureReason,
    );
    pub(super) fn drop_record(&self, signal: TelemetrySignal, reason: TelemetryDropReason);
}
```

Register `mod diagnostics;` in `src/telemetry.rs`.

- [ ] **Step 4: Verify GREEN and concurrency stability**

Run twice:

```bash
cargo test telemetry::diagnostics::tests
cargo test telemetry::diagnostics::tests
```

Expected: PASS both times; concurrent test reports exactly once and accounts for every suppression.

- [ ] **Step 5: Commit**

```bash
git add src/telemetry.rs src/telemetry/diagnostics.rs
git commit -m "feat: rate limit direct telemetry diagnostics"
```

---

### Task 3: Exact queue-full and pipeline-closed observation

**Files:**
- Modify: `src/telemetry/queue.rs`
- Modify: `src/telemetry.rs`

**Interfaces:**
- Consumes: `DiagnosticsObserver`, fixed signal, queue capacity.
- Preserves: existing `dropped_*`, `pending_*`, and failed-export runtime accessors.
- Produces: exact `queue_full` and `pipeline_closed` metrics/diagnostics.

- [ ] **Step 1: Write failing queue tests**

Refactor the queue test fixture to use real `Metrics` plus a test observer. Add trace and log coverage;
the production breaks caught are treating closure as saturation, delegating after closure, or losing
counts under contention.

```rust
#[test]
fn closed_admission_is_counted_as_pipeline_closed() {
    let metrics = Metrics::new();
    let observer = test_observer(metrics.clone());
    let boundary = AdmissionBoundary::new(2, TelemetrySignal::Trace, observer);

    boundary.close();
    assert_eq!(boundary.try_admit(), AdmissionOutcome::PipelineClosed);
    assert_eq!(boundary.dropped(), 1);
    assert!(metrics.encode().expect("metrics encode").contains(
        "github_telemetry_dropped_records_total{signal=\"trace\",reason=\"pipeline_closed\"} 1"
    ));
}
```

Extend the existing concurrent capacity test to assert the literal `queue_full` series equals
`CONTENDERS - CAPACITY`. Add processor-level tests proving `on_end`/`emit` after shutdown do not
reach a recording inner exporter.

- [ ] **Step 2: Run tests and verify RED**

Run: `cargo test telemetry::queue::tests`

Expected: compilation fails because boundaries do not accept observers, expose closure, or return a
typed outcome.

- [ ] **Step 3: Implement typed lock-free admission**

Add `AtomicBool closed` and:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AdmissionOutcome { Admitted, QueueFull, PipelineClosed }
```

Check `closed` before capacity CAS. On rejection, increment the existing aggregate dropped atomic and
call `observer.drop_record(signal, reason)` exactly once. On processor shutdown, set `closed` before
delegating. In `on_end` and `emit`, delegate only for `Admitted`. Keep pending release semantics and
all compare-exchange operations lock-free.

Pass cloned observers into `span_processor` and `log_processor`; update `build_trace_provider` and
`build_log_provider` to supply their fixed signal.

- [ ] **Step 4: Verify GREEN and existing occupancy behavior**

Run:

```bash
cargo test telemetry::queue::tests
cargo test telemetry::otlp_test::application_admission_prevents_sdk_queue_overflow
```

Expected: PASS; aggregate runtime drop totals and pending occupancy remain exact.

- [ ] **Step 5: Commit**

```bash
git add src/telemetry.rs src/telemetry/queue.rs
git commit -m "feat: observe bounded telemetry queue drops"
```

---

### Task 4: Structured OTLP HTTP and exporter failure classification

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `src/telemetry/http_client.rs`
- Modify: `src/telemetry.rs`
- Modify: `src/telemetry/queue.rs`

**Interfaces:**
- Consumes: one `DiagnosticsObserver` and fixed signal per exporter.
- Produces: `ObservingHttpClient<C: HttpClient>` implementing `HttpClient`.
- Produces: per-signal classified-attempt sequence used to prevent duplicate exporter accounting.

- [ ] **Step 1: Write failing HTTP classification tests**

In `http_client.rs`, use complete `http::Response` fakes only for deterministic protobuf response
validation. Use a real Reqwest blocking client against local `TcpListener` fixtures for timeout and
HTTP status behavior, plus an unused localhost port for connection refusal. Assert real observer
metrics, not client call counts. Add literal tests for:

- real local timeout -> `timeout`;
- real connection refusal -> `transport`;
- real non-success HTTP response -> `http_response`;
- valid empty trace/log protobuf -> success and zero failures;
- malformed successful trace/log protobuf -> returned redacted error and `encoding`;
- response body/header/endpoint sentinels absent from sink and exposition;
- a success after failure is returned normally.

The key malformed test is:

```rust
#[tokio::test]
async fn malformed_success_response_is_an_encoding_failure() {
    let (observer, metrics, sink) = observer_fixture();
    let client = ObservingHttpClient::new(
        StaticResponseClient::ok(vec![0xff, 0xff]),
        TelemetrySignal::Trace,
        observer,
    );
    let result = client.send_bytes(trace_request()).await;

    assert!(result.is_err());
    assert!(metrics.encode().expect("metrics encode").contains(
        "github_telemetry_export_failures_total{signal=\"trace\",reason=\"encoding\"} 1"
    ));
    assert!(!sink.text().contains("0xff"));
}
```

In `queue.rs`, add exporter-wrapper tests for `AlreadyShutdown`, `Timeout`, and `InternalFailure`, plus
one already-classified HTTP error proving it increments only once.

- [ ] **Step 2: Run tests and verify RED**

Run:

```bash
cargo test telemetry::http_client::tests
cargo test telemetry::queue::tests::export
```

Expected: compilation fails because the observing client and classified attempt state do not exist.

- [ ] **Step 3: Add direct production dependencies**

Move `opentelemetry-proto` and `prost` from dev-only use to normal dependencies with the existing
minimal trace/log features. Add:

```toml
opentelemetry-http = { version = "0.32", default-features = false, features = ["reqwest-blocking"] }
reqwest = { version = "0.13", default-features = false, features = ["blocking"] }
```

Keep OTLP metrics, gRPC, async Reqwest, and unrelated TLS features disabled.

- [ ] **Step 4: Implement the observing HTTP boundary**

Implement `HttpClient::send_bytes` by awaiting the inner client once. For errors, downcast to
`reqwest::Error`: `is_timeout()` maps to timeout, `status().is_some()` maps to HTTP response, and all
other request errors map to transport. Do not format the error. On success, decode the body as
`ExportTraceServiceResponse` or `ExportLogsServiceResponse` according to the fixed signal. Return a
boxed custom error whose `Display` is exactly `invalid OTLP response` on decode failure.

Increment an `AtomicU64 classified_attempts` after every client-side classification. Export wrappers
capture that sequence before delegating. If export returns an SDK error and the sequence is unchanged,
map `AlreadyShutdown`, `Timeout`, and `InternalFailure` exhaustively to shutdown, timeout, and internal.
Observe shutdown/force-flush errors through the same bounded mapping. Never parse SDK error strings.

Construct a Reqwest blocking client with `settings.timeout`; pass the observing client through
`.with_http_client(...)` for both OTLP exporters. Update `telemetry::init`/`build_runtime` to accept the
shared `Metrics`, construct one production observer, and pass it through both pipelines. Update every
test runtime builder call with an explicit test metrics value.

- [ ] **Step 5: Verify GREEN**

Run:

```bash
cargo test telemetry::http_client::tests
cargo test telemetry::queue::tests
cargo test telemetry::tests
```

Expected: PASS; every failure has one bounded classification and all emitted errors are redacted.

- [ ] **Step 6: Commit**

```bash
git add Cargo.toml Cargo.lock src/telemetry.rs src/telemetry/http_client.rs src/telemetry/queue.rs src/main.rs
git commit -m "feat: classify OTLP export failures"
```

---

### Task 5: End-to-end outage, saturation, privacy, and recursion guarantees

**Files:**
- Modify: `src/telemetry/otlp_test.rs`
- Modify: `docs/operations.md`
- Create: `changelog/2026-08-07T16-48-23Z-otlp-failure-drop-diagnostics.md`

**Interfaces:**
- Consumes: fully wired shared metrics and telemetry observer.
- Produces: acceptance-level regression evidence for issue #35.

- [ ] **Step 1: Write failing integration tests one behavior at a time**

Extend the existing in-process OTLP fixture rather than mocking Axum or the SDK. Add collector modes
for blocked response, non-success status with a private body, malformed `200` body, and recovery.
For each test, first name the production mutation it catches in a comment beside the setup.

Add tests proving:

1. connection refusal increments trace/log `transport` without changing readiness;
2. delayed response beyond configured timeout increments `timeout`;
3. `503` with a secret body increments `http_response` and leaks no body/status text;
4. malformed `200` increments `encoding` and recovery exports later batches;
5. tiny blocked queues produce exact trace/log `queue_full` totals and enqueue completes within a
   local timeout independent of collector release;
6. repeated same-category failures emit one direct line, while a different signal/reason emits its
   independent line;
7. direct diagnostics produce no OTLP log record and therefore no recursive failure;
8. an authenticated webhook still returns `204`, an invalid signature retains its current status,
   and `/health/ready` remains `200` throughout outage and saturation;
9. captured stderr, Prometheus exposition, traces, and logs exclude unique endpoint, authorization,
   response-body, transport, payload, signature, repository/workflow, delivery, SHA, and span-ID
   sentinels.

Use literal metric lines and HTTP statuses. Do not assert fake receiver call counts unless needed to
release a real blocked request.

- [ ] **Step 2: Run each new test and verify RED before production adjustment**

Run each exact test filter as it is added, for example:

```bash
cargo test telemetry::otlp_test::collector_outage_is_observed_without_affecting_http
cargo test telemetry::otlp_test::malformed_responses_are_encoding_failures_and_recover
cargo test telemetry::otlp_test::saturation_is_exact_non_blocking_and_non_recursive
```

Expected before its corresponding implementation is complete: FAIL on the missing metric,
diagnostic, or preserved behavior assertion—not fixture compilation or timing setup.

- [ ] **Step 3: Make only integration-driven corrections and document operations**

Correct classification/wiring defects exposed by RED tests without adding new categories or retry
behavior. Document both metric families, complete fixed labels, one-minute suppression semantics,
privacy rules, and the fact that collector failures never affect readiness or webhook acceptance.

Create the required timestamped changelog describing architecture, tests, privacy behavior, and
validation commands.

- [ ] **Step 4: Run focused acceptance tests**

Run:

```bash
cargo test telemetry::otlp_test
cargo test webhook_api
cargo test startup
```

Expected: PASS with no flaky timing assumptions or leaked sentinel values.

- [ ] **Step 5: Run the mandatory full validation sequence**

Run in order, restarting from the first command after any correction:

```bash
just fmt
cargo build
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps
```

Expected: every command exits zero with no warnings.

- [ ] **Step 6: Commit implementation documentation**

```bash
git add src/telemetry/otlp_test.rs docs/operations.md changelog
git commit -m "test: prove OTLP diagnostics resilience and privacy"
```

- [ ] **Step 7: Final issue-closing commit check**

If earlier commits do not contain the closing trailer, amend only the final commit message before
push:

```text
test: prove OTLP diagnostics resilience and privacy

Closes #35
```
