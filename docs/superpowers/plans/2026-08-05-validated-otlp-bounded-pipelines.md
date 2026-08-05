# Validated OTLP Configuration and Bounded Pipelines Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add optional, validated OTLP/HTTP trace and log export with independent stderr logging and exact application-owned non-blocking queue admission boundaries.

**Architecture:** `RuntimeConfig` owns a focused, redacted `TelemetryConfig` that resolves standard generic and signal-specific OTLP settings. `TelemetryRuntime` owns optional SDK tracer/logger providers and composes their tracing layers with the existing stderr layer. Thin admission processors and exporter wrappers share atomics around SDK dedicated-thread batch processors, making capacity and drop hooks observable without blocking request tasks or relying on private SDK counters.

**Tech Stack:** Rust 2021, tracing, OpenTelemetry 0.32, OTLP/HTTP protobuf, Tokio, Axum test receiver, Prost.

## Global Constraints

- Remote export is disabled when neither a generic nor signal-specific OTLP endpoint is configured.
- Each enabled signal defaults to a queue capacity of `2048`, maximum batch size of `512`, and telemetry shutdown timeout of five seconds.
- Queue capacity, batch size, and timeout are positive; batch size does not exceed capacity.
- Stderr logging remains active and uses the existing `EnvFilter` in enabled and disabled modes.
- OTLP headers, endpoint details, credentials, and invalid raw values never appear in errors or `Debug` output.
- Resources always contain `service.name` and package `service.version`; only approved pod and Kubernetes namespace attributes are accepted from `OTEL_RESOURCE_ATTRIBUTES`.
- OTLP metrics and broad service instrumentation remain out of scope.

---

### Task 1: Validated Telemetry Configuration

**Files:**
- Modify: `src/config.rs`

**Interfaces:**
- Consumes: the existing `RuntimeConfig::from_lookup` environment boundary.
- Produces: `TelemetryConfig`, `RuntimeConfig::telemetry()`, resolved per-signal exporter settings, queue capacity, batch size, shutdown timeout, service name, and approved resource attributes.

- [ ] **Step 1: Write failing configuration tests**

Add table-driven tests for documented defaults, valid generic and signal-specific overrides, disabled export, zero/malformed/overflowing integers, batch size greater than capacity, malformed/non-Unicode endpoints and headers, service-name defaulting, resource allowlisting, and redacted `Debug`/errors.

- [ ] **Step 2: Verify the tests fail for missing telemetry APIs**

Run: `cargo test config::tests::telemetry`
Expected: FAIL because `TelemetryConfig` and its accessors do not exist.

- [ ] **Step 3: Implement the minimal validated configuration boundary**

Parse only through the injected lookup, resolve signal-specific values over generic values, validate HTTP(S) endpoint URLs and HTTP header syntax, retain sensitive values only in private zeroizing storage, and filter resource attributes to `k8s.pod.name` and `k8s.namespace.name`.

- [ ] **Step 4: Verify configuration behavior**

Run: `cargo test config::tests`
Expected: PASS with all existing configuration tests unchanged.

### Task 2: Exact Non-Blocking Admission Boundaries

**Files:**
- Create: `src/telemetry/queue.rs`
- Modify: `src/telemetry.rs`

**Interfaces:**
- Consumes: validated queue capacity and SDK `SpanProcessor`, `LogProcessor`, `SpanExporter`, and `LogExporter` traits.
- Produces: trace/log processor wrappers whose synchronous entrypoints use atomic compare-exchange admission, exact drop counters, and exporter wrappers that release admitted capacity when a batch starts export.

- [ ] **Step 1: Write failing queue tests**

Test that admission through capacity succeeds, the next admission drops immediately, releasing a batch restores exactly that capacity, concurrent admission never exceeds the configured bound, and every observed batch length is at most the configured maximum.

- [ ] **Step 2: Verify the tests fail for the missing queue module**

Run: `cargo test telemetry::queue::tests`
Expected: FAIL because the queue boundary does not exist.

- [ ] **Step 3: Implement the atomic admission boundary and SDK adapters**

Use `AtomicUsize::compare_exchange_weak` for lock-free non-blocking admission and `AtomicU64` for exact drop hooks. Delegate admitted records to SDK dedicated-thread batch processors; decrement pending records at exporter batch start so SDK queues cannot overflow behind the application boundary.

- [ ] **Step 4: Verify queue behavior**

Run: `cargo test telemetry::queue::tests`
Expected: PASS, including the concurrent capacity assertion.

### Task 3: Optional Telemetry Runtime and Independent Layers

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `src/telemetry.rs`
- Modify: `src/main.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `TelemetryConfig` and queue adapters from Tasks 1 and 2.
- Produces: `TelemetryRuntime::init`, `TelemetryRuntime::state`, owned optional tracer/logger providers, local stderr output in both states, and internal hooks for later drop/failure metrics and shutdown work.

- [ ] **Step 1: Write failing runtime/subscriber tests**

Assert disabled construction has no providers, enabled construction owns both providers, and a captured writer receives the same filtered application event with remote export enabled or disabled. Assert OpenTelemetry-internal targets are excluded from the remote log layer.

- [ ] **Step 2: Verify runtime tests fail**

Run: `cargo test telemetry::tests`
Expected: FAIL because `TelemetryRuntime` and enabled-layer composition do not exist.

- [ ] **Step 3: Add minimal-feature dependencies and runtime implementation**

Enable OpenTelemetry trace/log APIs, OTLP HTTP protobuf with the blocking Reqwest client used only by dedicated exporter threads, tracing bridges, and no gRPC or OTLP metrics features. Build resources with package version, providers with bounded batch processors, and a subscriber whose stderr layer remains independent.

- [ ] **Step 4: Wire startup ownership and verify runtime tests**

Have `main` retain `TelemetryRuntime` for process lifetime without adding final shutdown integration. Run: `cargo test telemetry::tests`
Expected: PASS.

### Task 4: OTLP/HTTP Protobuf Integration and Documentation

**Files:**
- Create: `tests/otlp_pipeline.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `changelog/2026-08-05T<time>-validated-otlp-bounded-pipelines.md`

**Interfaces:**
- Consumes: the enabled telemetry runtime and test-only in-process Axum receiver.
- Produces: regression proof that one synthetic span and one synthetic log reach `/v1/traces` and `/v1/logs` as valid protobuf while stderr remains active.

- [ ] **Step 1: Write the failing receiver integration test**

Start an in-process receiver on an ephemeral address, decode `ExportTraceServiceRequest` and `ExportLogsServiceRequest`, construct the runtime against it, emit one named span and one named event, force-flush providers through a test hook, and assert both decoded requests contain records and approved resource fields.

- [ ] **Step 2: Verify the integration test fails before final test support is present**

Run: `cargo test --test otlp_pipeline`
Expected: FAIL because receiver-facing test construction/flush support is incomplete.

- [ ] **Step 3: Complete focused test support and changelog**

Add only protobuf decoding dependencies needed by tests, expose crate-private/test-safe flush hooks without integrating production shutdown, and document configuration, architecture, tests, and deferred work.

- [ ] **Step 4: Run the full validation sequence**

Run in order: `just fmt`; `cargo build`; `cargo clippy --all-targets -- -D warnings`; `just test`; `cargo doc --no-deps`.
Expected: every command exits successfully with no warnings.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src tests docs/superpowers/plans changelog
git commit -m "feat: add bounded OTLP signal pipelines

Closes #33"
```
