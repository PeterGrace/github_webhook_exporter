# Bounded Prometheus Metrics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add bounded-cardinality webhook metrics and an unauthenticated Prometheus exposition endpoint.

**Architecture:** A focused `metrics` module owns a registry and all instruments behind one `Arc`. Closed Rust enums and pure normalizers are the only path from untrusted event/action input to labels; narrow methods update related instruments without exposing the registry.

**Tech Stack:** Rust 2021, Axum 0.8, `prometheus-client` 0.25 with no optional features, Tokio, Tower tests.

## Global Constraints

- Use the exact event, action, result, and failure-stage vocabularies from Specification 2.
- Missing actions map to `none`; every unrecognized present event/action value maps to `other`.
- Never use repository names, delivery IDs, payload fields, signatures, URLs, PR numbers, SHAs, or raw event/action strings as labels.
- Keep webhook orchestration and repository-gauge lifecycle wiring out of scope.
- Add a timestamped Markdown entry under `changelog/`.

---

### Task 1: Closed label normalization

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `src/metrics.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: `normalize_event_type(&str) -> EventType`
- Produces: `normalize_action(Option<&str>) -> Action`
- Produces: closed public enums `WebhookResult` and `FailureStage`

- [x] **Step 1: Add table-driven failing tests**

Add tests in `src/metrics.rs` that enumerate the 21 accepted event strings and 26 accepted action strings, assert each exact value is preserved, and assert unknown, empty, mixed-case, and malicious strings normalize to `other`. Assert `None` normalizes to `none` while `Some("")` normalizes to `other`.

- [x] **Step 2: Run the normalization tests and verify RED**

Run: `cargo test metrics::tests::normalization -- --nocapture`

Expected: compilation fails because the module and normalization API do not exist.

- [x] **Step 3: Add the dependency and minimal closed types**

Run: `cargo add prometheus-client@0.25.0 --no-default-features`

Implement `EventType` and `Action` as private-field-safe enums deriving `EncodeLabelValue`; implement exact-match normalizers with exhaustive `match` arms. Implement `WebhookResult` and `FailureStage` as public closed enums deriving `Clone`, `Copy`, `Debug`, `Hash`, `PartialEq`, `Eq`, and `EncodeLabelValue`.

- [x] **Step 4: Export the module and verify GREEN**

Add `pub mod metrics;` with a module doc comment in `src/lib.rs`.

Run: `cargo test metrics::tests::normalization -- --nocapture`

Expected: all normalization tests pass.

### Task 2: Cloneable metrics component and narrow updates

**Files:**
- Modify: `src/metrics.rs`

**Interfaces:**
- Produces: `Metrics::new() -> Metrics`
- Produces: `Metrics::observe_request(WebhookResult, Duration)`
- Produces: `Metrics::observe_event(EventType, Action, usize)`
- Produces: `Metrics::record_duplicate()`
- Produces: `Metrics::record_failure(FailureStage)`
- Produces: `Metrics::set_repository_configurations(u64)`
- Produces: crate-private `Metrics::encode() -> Result<String, fmt::Error>`

- [x] **Step 1: Write failing metric-update tests**

Add tests that create `Metrics`, invoke every narrow method, encode the registry, and assert:

```text
github_webhook_requests_total{result="accepted"} 1
github_webhook_events_total{event_type="push",action="none"} 1
github_webhook_processing_duration_seconds_count{result="accepted"} 1
github_webhook_request_body_bytes_count 1
github_webhook_duplicates_total 1
github_webhook_processing_failures_total{stage="metrics"} 1
github_repository_configurations 7
```

Also normalize deliberately sensitive/raw inputs before observation and assert none appear in encoded output.

- [x] **Step 2: Run update tests and verify RED**

Run: `cargo test metrics::tests::metric_updates -- --nocapture`

Expected: compilation fails because `Metrics` and its methods do not exist.

- [x] **Step 3: Implement the minimal component**

Create `Metrics { inner: Arc<MetricsInner> }`. Register families under a `Registry::with_prefix("github")`; use counter families for request/event/failure labels, histogram families for result-labelled duration, a histogram for body bytes, a counter for duplicates, and `Gauge<u64, AtomicU64>` for repository count. Use fixed duration/body buckets. Keep every instrument and the registry private.

- [x] **Step 4: Verify metric updates GREEN**

Run: `cargo test metrics::tests::metric_updates -- --nocapture`

Expected: all metric-update and forbidden-value tests pass.

- [x] **Step 5: Write and verify a failing shared-clone concurrency test**

Spawn several threads from cloned `Metrics`, increment duplicates and one normalized event per thread, join them, and assert the single encoded registry contains the exact aggregate counts.

Run: `cargo test metrics::tests::clones_share -- --nocapture`

Expected before the shared implementation is complete: aggregate assertion fails.

- [x] **Step 6: Make clone sharing pass without an application lock**

Ensure all clones share one `Arc<MetricsInner>` and rely only on `prometheus-client` synchronization.

Run: `cargo test metrics::tests::clones_share -- --nocapture`

Expected: concurrency test passes.

### Task 3: Axum exposition route and application state

**Files:**
- Modify: `src/metrics.rs`
- Modify: `src/app.rs`

**Interfaces:**
- Produces: `metrics::router() -> Router<AppState>`
- Consumes: `AppState::metrics() -> &Metrics`
- Produces: `GET /metrics` returning OpenMetrics text with status 200

- [x] **Step 1: Write failing router tests**

Add tests that build the application router, request `GET /metrics`, and assert status `200 OK`, content type `application/openmetrics-text; version=1.0.0; charset=utf-8`, and the seven required metric families. Assert no authorization header is required.

- [x] **Step 2: Run router test and verify RED**

Run: `cargo test metrics_endpoint -- --nocapture`

Expected: response is `404 Not Found`.

- [x] **Step 3: Wire metrics into state and add the focused router**

Add private `metrics: Metrics` to `AppState`, initialize it in `AppState::new`, and expose a documented borrowed accessor. Add an async handler returning `Result<Response, AppError>`; encode into a `String`, set the OpenMetrics content type, and return a safe internal error if encoding fails. Merge the focused router in `build_router`.

- [x] **Step 4: Verify router GREEN**

Run: `cargo test metrics_endpoint -- --nocapture`

Expected: endpoint tests pass.

### Task 4: Documentation and complete validation

**Files:**
- Create: `changelog/<timestamp>-bounded-prometheus-metrics.md`
- Modify as needed: files changed above

- [x] **Step 1: Add the timestamped changelog**

Document bounded normalization, the narrow shared metrics API, `/metrics`, and the tests added.

- [x] **Step 2: Run the full required gate from the top**

Run in order:

```bash
just fmt
cargo build
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps
```

Expected: every command exits zero without warnings.

- [x] **Step 3: Exercise the delivered endpoint artifact**

Start the binary with valid temporary environment values, request `/metrics`, verify `200`, the OpenMetrics content type, and all required names, then stop the process.

- [x] **Step 4: Commit the scoped change**

```bash
git add Cargo.toml Cargo.lock src/lib.rs src/metrics.rs src/app.rs docs/superpowers/plans changelog
git commit -m "feat: add bounded Prometheus metrics

Closes #14"
```
