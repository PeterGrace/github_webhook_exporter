# PR #80 Final Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make optional Sentry Issue promotion functional, deadline-safe, stably grouped, and privacy-regression-tested without changing canonical OTLP behavior or span linkage.

**Architecture:** Build the Sentry client through the supported reqwest transport options with an application-bounded HTTP client and reject disabled clients. Run trace, log, and Sentry shutdown operations in detached application-owned workers under one receiver deadline, with direct redacted Sentry diagnostics. Keep task-run-ID display fallbacks separate from stable fingerprint fields, and inject `TestTransport` only through the runtime's internal construction seam for no-network acceptance coverage.

**Tech Stack:** Rust 2021, Sentry Rust SDK 0.49.1, reqwest 0.13, OpenTelemetry SDK/OTLP 0.32, Tokio, built-in Rust tests.

## Global Constraints

- Canonical OTLP `exception` events remain primary and are recorded before optional Sentry reporting.
- Default Sentry integrations, automatic sessions, and default PII remain disabled.
- Sentry HTTP requests have a finite timeout, and `TelemetryRuntime::shutdown` returns by its shared outer deadline even when Sentry blocks.
- Shutdown workers are detached on the deadline path; the caller never joins a blocked Sentry transport.
- Diagnostics contain only bounded provider/reason vocabulary and no DSN, endpoint, SDK error, payload, or credential text.
- Fingerprints include task kind and stable sanitized identities; per-run job IDs are not grouping components.
- Exact OTLP/Sentry trace and span IDs remain unchanged.
- Failed/timed-out children continue to suppress job fallback; reporter availability never changes OTLP emission or duplicate suppression.
- Production code uses no `unwrap()`.

---

### Task 1: Enable the production Sentry transport

**Files:**
- Modify: `src/telemetry.rs`
- Test: `src/telemetry.rs`

**Interfaces:**
- Consumes: validated `SENTRY_DSN`, service name, and the configured OTLP trace request timeout.
- Produces: an enabled `SentryErrorClient` using `ReqwestHttpTransportOptions` and an injectable `Arc<dyn TransportFactory>` construction seam.

- [ ] **Step 1: Add a production-construction regression test**

Add `configured_sentry_client_is_enabled` to call the real private builder with a valid DSN and assert:

```rust
assert!(client.0.is_enabled());
assert!(!client.0.options().default_integrations);
assert!(!client.0.options().auto_session_tracking);
assert!(!client.0.options().send_default_pii);
```

The mutation caught is removing the transport factory or restoring privacy-sensitive defaults.

- [ ] **Step 2: Run the exact test and record RED**

Run:

```bash
cargo test telemetry::tests::configured_sentry_client_is_enabled -- --exact --nocapture
```

Expected: FAIL because `Client::from_config` receives no transport and `is_enabled()` is false.

- [ ] **Step 3: Install the explicit supported bounded transport**

Implement a private cloneable factory:

```rust
impl sentry::TransportFactory for SentryReqwestTransportFactory {
    fn create_transport_with_options(
        &self,
        options: sentry::TransportOptions,
    ) -> Arc<dyn sentry::Transport> {
        Arc::new(
            sentry::transports::ReqwestHttpTransportOptions::from(options)
                .with_client(self.client.clone())
                .build(),
        )
    }
}
```

Build `reqwest::Client` with the validated trace timeout, set the factory explicitly on `ClientOptions`, set `auto_session_tracking` directly to false so non-test builds do not require the `release-health` feature, construct the client, and return `TelemetryError::SentryClient` if it is disabled.

- [ ] **Step 4: Run focused GREEN**

Run the exact test plus `cargo check`; both must exit zero.

---

### Task 2: Bound Sentry shutdown and report terminal failures

**Files:**
- Modify: `src/telemetry.rs`
- Modify: `src/telemetry/diagnostics.rs`
- Test: `src/telemetry.rs`
- Test: `src/telemetry/diagnostics.rs`

**Interfaces:**
- Consumes: trace/log providers, optional `SentryErrorClient`, one timeout, and `DiagnosticsObserver`.
- Produces: `run_shutdown_tasks(Vec<ShutdownTask>, timeout, diagnostics)` handling `Trace`, `Log`, and `Sentry` task kinds under one receiver deadline.

- [ ] **Step 1: Add blocking and failing Sentry transport tests**

Add deterministic transports implementing `sentry::Transport`:

```rust
fn shutdown(&self, _timeout: Duration) -> bool {
    thread::sleep(Duration::from_millis(250));
    true
}
```

and:

```rust
fn shutdown(&self, _timeout: Duration) -> bool {
    false
}
```

Assert a 25 ms runtime shutdown returns `TimedOut` in substantially less than 250 ms and emits exactly:

```text
telemetry pipeline diagnostic kind=failure signal=sentry reason=timeout
```

Assert the failing transport returns `Failed` and emits exactly one redacted `reason=shutdown` Sentry diagnostic.

- [ ] **Step 2: Run the exact tests and record RED**

Expected current behavior: the blocking test overruns the deadline and reports `Completed`; the failure path produces no diagnostic.

- [ ] **Step 3: Extend the worker coordinator**

Introduce a private three-variant shutdown task kind. Spawn every operation through the existing named detached-worker pattern, wait only on the receiver until the shared deadline, and classify unfinished Sentry work as `timeout`. Move `client.close(Some(remaining))` into the Sentry worker closure before calling `run_shutdown_tasks`.

- [ ] **Step 4: Add direct bounded Sentry diagnostics**

Add a `DiagnosticsObserver` method accepting only `Shutdown` or `Timeout`, render the fixed `signal=sentry` vocabulary directly to the sink, and do not add `sentry` to the OTLP-only Prometheus `signal=trace|log` series.

- [ ] **Step 5: Run focused GREEN**

Run all telemetry shutdown and diagnostics tests and confirm exact outcomes, elapsed bounds, and output.

---

### Task 3: Separate presentation and grouping identities

**Files:**
- Modify: `src/telemetry/workflow_error.rs`
- Modify: `book/src/reference/traces.md`
- Test: `src/telemetry/workflow_error.rs`

**Interfaces:**
- Consumes: sanitized optional workflow/job/task names, task kind, positive step ordinal, conclusion, and validated run IDs.
- Produces: unchanged display fields plus a seven-component stable fingerprint containing task kind.

- [ ] **Step 1: Add stable unnamed-task and kind-separation tests**

Create equivalent unnamed jobs/steps with different job IDs and assert their display fallbacks differ while fingerprints are equal. Create a job fallback and step with equal displayed names and assert fingerprints differ.

- [ ] **Step 2: Run the exact tests and record RED**

Expected: unnamed fingerprints differ because they contain job IDs, and equal-name job/step fingerprints compare equal because kind is absent.

- [ ] **Step 3: Store separate grouping fields**

Retain `task_name` and `task_run_id` for descriptions/tags. Add stable grouping fields using sanitized names, `unnamed-job`, and `unnamed-step:<positive ordinal>`. Return:

```rust
[
    "github-actions-task",
    self.kind.as_str(),
    &self.repository_name,
    &self.workflow_name,
    &self.grouping_job_name,
    &self.grouping_task_name,
    self.conclusion,
]
```

- [ ] **Step 4: Run focused GREEN and existing linkage/duplication tests**

Verify fingerprints, exact trace/span IDs, one event per failed child, and no job duplicate.

---

### Task 4: Add hostile failing-payload privacy acceptance coverage

**Files:**
- Modify: `src/telemetry.rs`
- Modify: `src/telemetry/otlp_test.rs`
- Test: `src/telemetry/otlp_test.rs`

**Interfaces:**
- Consumes: a raw completed failing `workflow_job` webhook and an injected Sentry `TestTransport`.
- Produces: serialized OTLP requests and a captured Sentry event without network access.

- [ ] **Step 1: Add the no-network fixture seam**

Add a test-only runtime wrapper that supplies `Arc<dyn TransportFactory>` to the common runtime builder. Configure `SENTRY_DSN` only in the new fixture and return the concrete `TestTransport` alongside it.

- [ ] **Step 2: Add the hostile failing payload test**

Submit a failing payload containing control characters, overlong names, command/output/log/actor/URL/secret/signature/header/raw-fragment sentinels. Assert exactly one failed step exception, exact two OTLP event attributes and historical linkage, exact allowlisted Sentry exception/context/tags/fingerprint fields, and absence of every prohibited sentinel from serialized OTLP bytes and serialized captured Sentry event JSON.

The production mutations caught are adding raw payload fields to either projection, changing linkage, or dropping either representation.

- [ ] **Step 3: Record RED through the pre-fix runtime path and privacy mutation check**

Before production transport changes, the production-built client is disabled. For the structurally private projection that already exists, temporarily inject one prohibited sentinel into the Sentry event, run the exact hostile test to prove it fails, then revert the mutation before GREEN.

- [ ] **Step 4: Run focused GREEN**

Run the exact hostile test plus workflow OTLP/reporter suites; no network request may be made.

---

### Task 5: Reconcile documentation, validate, report, and commit

**Files:**
- Modify: `changelog/2026-08-12T19-34-33Z-otlp-workflow-exception-events-task-1.md`
- Create: `changelog/2026-08-12T20-05-51Z-pr-80-final-review-fixes.md`
- Modify: `book/src/reference/telemetry.md`
- Modify: `book/src/reference/traces.md`
- Create: `.superpowers/sdd/2026-08-12-otlp-workflow-exception-events/final-fix-report.md`

**Interfaces:**
- Consumes: RED/GREEN logs, final diff, and validation command outputs.
- Produces: accurate release/operator documentation, the required final report, and local commits only.

- [ ] **Step 1: Reconcile stale documentation**

Mark the task-1 validation failures as transient and resolved. Document concurrent deadline-governed Sentry shutdown and stable grouping by task kind/name/ordinal rather than run ID. Add the timestamped changelog entry.

- [ ] **Step 2: Run focused and full practical validation**

Run:

```bash
cargo fmt --all
cargo test telemetry::tests::configured_sentry_client_is_enabled -- --exact
cargo test telemetry::tests::sentry_shutdown_ -- --nocapture
cargo test telemetry::workflow_error::tests:: -- --nocapture
cargo test telemetry::otlp_test::hostile_failed_workflow_payload_is_private -- --exact --nocapture
cargo test telemetry::workflow::tests:: -- --nocapture
cargo clippy --all-targets -- -D warnings
cargo test
cargo fmt --all -- --check
git diff --check
```

Run `just test` too if it adds practical coverage beyond `cargo test` within the available time.

- [ ] **Step 3: Self-review invariants**

Inspect the diff to confirm OTLP event creation still precedes optional reporting, child fallback suppression is reporter-independent, trace/span IDs are copied exactly, no production `unwrap()` was added, no secret text appears in diagnostics, and no join occurs on the caller's deadline path.

- [ ] **Step 4: Write the final report**

Record locked-source root-cause line evidence, exact RED/GREEN commands and outputs, files, commits, self-review, and residual live-Sentry/network concerns.

- [ ] **Step 5: Commit locally without pushing or editing PR #80**

Create clear local commits for runtime/lifecycle, grouping/privacy, and documentation/reporting. Verify `origin/feat-issue-78-linked-sentry-errors` remains at the pre-fix SHA.
