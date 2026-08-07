# Telemetry Provider Shutdown Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Flush and shut down enabled OTLP trace and log providers exactly once within one shared deadline on every post-initialization process exit.

**Architecture:** `TelemetryRuntime` will close both application admission boundaries, move each provider into one dedicated shutdown thread, and collect both results until one monotonic deadline. The process lifecycle will separate post-telemetry startup/serving from top-level cleanup so normal signals, server errors, startup errors, and HTTP drain timeouts all reach the same telemetry shutdown operation. Existing bounded diagnostics will account for providers that fail or remain unfinished at the shared deadline without exposing SDK errors.

**Tech Stack:** Rust 2021, Tokio, OpenTelemetry SDK 0.32, Axum, Prometheus client, in-process OTLP/HTTP test receiver.

## Global Constraints

- Trace and log shutdown share `GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS`, defaulting to five seconds.
- HTTP and retention continue to share only `GHE_SHUTDOWN_TIMEOUT_SECONDS`.
- Telemetry failures never change a previously graceful service result into a process failure.
- Direct diagnostics use only fixed kind, signal, reason, and numeric suppression fields.
- Span-only identifiers remain forbidden from stderr, OTLP logs, and Prometheus exposition.
- Secrets, signatures, authorization/OTLP headers, bodies, payload fragments, commands, actors, raw URLs, and step logs appear nowhere.
- No new span family is introduced.

---

### Task 1: Shared-deadline idempotent provider shutdown

**Files:**
- Modify: `src/telemetry.rs`
- Modify: `src/telemetry/queue.rs`
- Test: `src/telemetry.rs`
- Test: `src/telemetry/queue.rs`

**Interfaces:**
- Consumes: `SdkTracerProvider::shutdown_with_timeout(Duration)`, `SdkLoggerProvider::shutdown_with_timeout(Duration)`, `AdmissionBoundary::close()`.
- Produces: `TelemetryRuntime::shutdown(&mut self, Duration) -> TelemetryShutdownOutcome` and public bounded `TelemetryShutdownOutcome::{Completed, Failed, TimedOut}`.

- [ ] **Step 1: Write failing controlled shutdown tests**

Add tests that drive a private shared-deadline helper with trace/log closures. Use barriers/channels to prove both closures start concurrently, one hanging closure cannot prevent the other from completing, the result is `TimedOut`, and elapsed time is bounded by one deadline rather than two serial deadlines. Add a runtime test proving a second shutdown call performs no provider operation and returns the same terminal outcome.

- [ ] **Step 2: Run the focused tests and verify failure**

Run: `cargo test telemetry::tests::shutdown -- --nocapture`

Expected: compilation fails because the shutdown helper and outcome do not exist.

- [ ] **Step 3: Implement minimal shutdown orchestration**

Close both admission boundaries before moving providers out of the runtime. Spawn one named native thread per enabled signal, pass each provider the same timeout, and collect signal-tagged results only until `Instant::now() + timeout`. Record a normalized timeout diagnostic for each unfinished signal. Store the terminal outcome so repeated calls are no-ops with the same result. Never render an SDK error.

- [ ] **Step 4: Revalidate admission accounting**

Add a queue test that admits a record, closes the boundary, releases the accepted record, and proves `pending == 0`; then prove a post-close record increments exactly one `pipeline_closed` drop. Update the queue lifecycle documentation to state the final ordering.

- [ ] **Step 5: Run focused telemetry tests**

Run: `cargo test telemetry:: -- --nocapture`

Expected: all telemetry unit tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/telemetry.rs src/telemetry/queue.rs
git commit -m "feat: add bounded telemetry provider shutdown"
```

### Task 2: Process lifecycle integration

**Files:**
- Modify: `src/main.rs`
- Test: `tests/startup.rs`

**Interfaces:**
- Consumes: `TelemetryRuntime::shutdown`, `TelemetryConfig::shutdown_timeout`, `app::serve_with_shutdown`.
- Produces: one top-level post-initialization cleanup path that preserves the service `Result` independently of telemetry shutdown outcome.

- [ ] **Step 1: Write failing process lifecycle assertions**

Extend the SIGINT/SIGTERM process fixture with an enabled local OTLP endpoint and `GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS=1`. Assert both signals still exit successfully, include the normalized provider-shutdown completion message, and complete inside the sum of the HTTP/retention and telemetry boundaries. Add a startup failure after telemetry initialization and assert it exits nonzero only for the startup error while telemetry cleanup runs and configured header values remain absent from stderr.

- [ ] **Step 2: Run focused startup tests and verify failure**

Run: `cargo test --test startup -- --nocapture`

Expected: new shutdown lifecycle assertions fail because `main` does not explicitly shut down telemetry.

- [ ] **Step 3: Refactor startup/serving under an always-cleanup boundary**

Keep configuration and telemetry initialization at the top. Move all subsequent startup and serving work into a helper returning `anyhow::Result<()>`. Await it, then await telemetry shutdown using the separately configured telemetry deadline. Log only fixed completion, failure, or timeout messages and return the original service result unchanged.

- [ ] **Step 4: Run startup tests**

Run: `cargo test --test startup -- --nocapture`

Expected: startup, SIGINT, and SIGTERM tests pass with redacted stderr.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs tests/startup.rs
git commit -m "feat: integrate telemetry into process shutdown"
```

### Task 3: End-to-end OTLP shutdown and privacy regressions

**Files:**
- Modify: `src/telemetry/otlp_test.rs`
- Modify: `tests/startup.rs`

**Interfaces:**
- Consumes: existing `OtlpFixture`, router/process helpers, repository configuration API, webhook fixtures, readiness and metrics endpoints.
- Produces: regression evidence that shutdown exports accepted core/workflow telemetry and that hostile values remain confined by policy.

- [ ] **Step 1: Write an accepted-at-shutdown export test**

Configure both signals against the in-process receiver, emit a core request and completed workflow job without calling `force_flush`, invoke runtime shutdown, and assert the receiver captures required core spans, workflow root/step spans, and application logs. Assert trace/log pending counts return to zero or any rejected post-close record is counted as `pipeline_closed`.

- [ ] **Step 2: Run the focused test and verify failure**

Run: `cargo test telemetry::otlp_test::shutdown -- --nocapture`

Expected: the test fails until it uses the new shutdown lifecycle and receiver capture path.

- [ ] **Step 3: Add the complete output privacy matrix**

Scan OTLP spans, OTLP logs, captured stderr, Prometheus exposition, HTTP responses, and relevant SQLite bytes/rows. Permit approved identifiers only in spans. Include hostile secrets, signatures, auth and OTLP header values, payload fragments, commands, actor names, raw URLs, and step logs and assert none appear in any output.

- [ ] **Step 4: Add collector-outage shutdown coverage**

Point both exporters at an unavailable endpoint, accept a readiness request and authenticated webhook, then shut down. Assert timely completion, unchanged successful HTTP results, bounded failure metrics, and redacted normalized stderr diagnostics.

- [ ] **Step 5: Run focused integration tests**

Run: `cargo test telemetry::otlp_test:: -- --nocapture`

Expected: OTLP lifecycle and privacy tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/telemetry/otlp_test.rs tests/startup.rs
git commit -m "test: cover telemetry shutdown end to end"
```

### Task 4: Operational and public API documentation

**Files:**
- Modify: `docs/operations.md`
- Modify: `src/telemetry.rs`
- Modify: `src/lib.rs` if the public module summary needs lifecycle wording
- Create: `changelog/2026-08-07T17-56-45-0400-telemetry-provider-shutdown.md`

**Interfaces:**
- Consumes: final runtime behavior and stable configuration/metric vocabularies.
- Produces: operator contract for enabled/disabled telemetry, endpoints/headers, bounds, hierarchy, policies, failures, counters, and shutdown ordering.

- [ ] **Step 1: Update operations documentation**

Replace the reserved shutdown setting text with its enforced shared-deadline contract. Document disabled mode, endpoint/header examples with percent encoding, queue and batch behavior, span hierarchy, merge-queue and workflow limitations, identifier boundaries, collector failures, diagnostic counters, and HTTP/retention-before-telemetry ordering. State that identifiers in local or OTLP application logs remain a deferred policy choice.

- [ ] **Step 2: Update API documentation and changelog**

Document parameters, return value, idempotence, timeout behavior, and non-propagating errors on public shutdown APIs. Add the timestamped changelog entry summarizing lifecycle, accounting, regression tests, and documentation.

- [ ] **Step 3: Build documentation**

Run: `cargo doc --no-deps`

Expected: documentation builds without warnings.

- [ ] **Step 4: Commit**

```bash
git add docs/operations.md src/telemetry.rs src/lib.rs changelog/2026-08-07T17-56-45-0400-telemetry-provider-shutdown.md
git commit -m "docs: define telemetry shutdown operations contract"
```

### Task 5: Full validation and delivery

**Files:**
- Modify only files required to fix validation findings.

**Interfaces:**
- Consumes: all prior tasks.
- Produces: a review-ready branch and pull request closing issue #36.

- [ ] **Step 1: Run the mandatory gate from the top**

```bash
just fmt
cargo build
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps
```

Expected: every command exits successfully without warnings.

- [ ] **Step 2: Review the diff and privacy-sensitive strings**

Run: `git diff origin/main...HEAD --check` and inspect `git diff origin/main...HEAD`. Confirm no raw SDK error formatting, credentials, fixture secrets outside assertions/input fixtures, or unrelated refactors entered production output.

- [ ] **Step 3: Commit validation fixes if needed**

Use a scoped commit message that names the corrected behavior; if no fixes are needed, do not create an empty commit.

- [ ] **Step 4: Push and open the PR**

Push `github-webhook-exporter/gwe-36`, open a PR against `main` titled `feat: flush providers and complete observability regressions`, include actual validation results, and use `Closes #36`.

- [ ] **Step 5: Link the PR on issue #36**

Comment with the created PR number and URL.
