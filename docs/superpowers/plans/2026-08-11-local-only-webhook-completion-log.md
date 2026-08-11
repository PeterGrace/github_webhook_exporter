# Local-Only Webhook Completion Log Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Emit every `GitHub webhook request processed` event as a local DEBUG diagnostic without exporting it through OpenTelemetry logs.

**Architecture:** Introduce one explicit local-only tracing target and a log-specific OTLP metadata filter that rejects it while preserving all other application log policy. Move the generic webhook completion event to DEBUG on that target, leaving metrics, traces, and specialized warning/error events unchanged.

**Tech Stack:** Rust 2021, `tracing`, `tracing-subscriber`, OpenTelemetry tracing bridge, Tokio, Axum, in-process OTLP protobuf receiver.

## Global Constraints

- Apply the policy to every generic `GitHub webhook request processed` event regardless of response status.
- Keep the event available to local structured logging when `RUST_LOG` enables DEBUG.
- Never admit the local-only target to the OpenTelemetry log pipeline, even when DEBUG is enabled.
- Do not globally suppress other DEBUG OTLP records.
- Do not change webhook responses, request metrics, traces, or specialized warning/error events.
- Add no dependencies and perform no unrelated refactoring.

## File Structure

- `src/telemetry.rs`: owns the local-only target and the OTLP log metadata admission policy.
- `src/api/webhook.rs`: emits the generic completion event at DEBUG on the local-only target.
- `src/telemetry/otlp_test.rs`: proves the local/remote behavior through the real webhook and OTLP pipeline.
- `changelog/2026-08-11T10-11-33-0400-local-only-webhook-completion-logs.md`: records behavior and validation.

---

### Task 1: Add the local-only OTLP log admission policy

**Files:**
- Modify: `src/telemetry.rs:34-45,398-405,537-550,568-575,742-751`
- Test: `src/telemetry.rs`

**Interfaces:**
- Consumes: existing `is_application_target(target: &str) -> bool` application namespace policy.
- Produces: `pub(crate) const LOCAL_ONLY_LOG_TARGET: &str` and private `is_remote_log_target(target: &str) -> bool`, used by the OTLP log metadata filter and webhook emitter.

- [ ] **Step 1: Write the failing target-policy test**

Extend the `telemetry::tests` imports to include `is_remote_log_target` and `LOCAL_ONLY_LOG_TARGET`, then add:

```rust
#[test]
fn remote_log_filter_rejects_only_the_local_only_application_target() {
    assert!(!is_remote_log_target(LOCAL_ONLY_LOG_TARGET));
    assert!(is_remote_log_target("github_webhook_exporter"));
    assert!(is_remote_log_target("github_webhook_exporter::api"));
    assert!(!is_remote_log_target("unrelated_dependency"));
}
```

The ordinary application target assertions prove this is not a global DEBUG suppression policy.

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```bash
cargo test telemetry::tests::remote_log_filter_rejects_only_the_local_only_application_target --lib -- --exact
```

Expected: compilation fails because `is_remote_log_target` and `LOCAL_ONLY_LOG_TARGET` do not yet exist.

- [ ] **Step 3: Implement the minimal target policy and wire the OTLP log layer**

Near `INSTRUMENTATION_SCOPE`, add the internal documented constant:

```rust
/// Tracing target for local diagnostics that must never enter the OTLP log pipeline.
pub(crate) const LOCAL_ONLY_LOG_TARGET: &str = "github_webhook_exporter::local_only";
```

Add a log-specific metadata predicate while preserving the trace predicate:

```rust
fn application_log_metadata(metadata: &Metadata<'_>) -> bool {
    is_remote_log_target(metadata.target())
}

fn application_metadata(metadata: &Metadata<'_>) -> bool {
    is_application_target(metadata.target())
}

fn application_trace_metadata(metadata: &Metadata<'_>) -> bool {
    metadata.is_span() && application_metadata(metadata)
}

fn is_remote_log_target(target: &str) -> bool {
    target != LOCAL_ONLY_LOG_TARGET && is_application_target(target)
}
```

Change only the OpenTelemetry log layer to use the new predicate:

```rust
let log_layer = logger_provider.as_ref().map(|provider| {
    OpenTelemetryTracingBridge::new(provider)
        .with_filter(filter_fn(application_log_metadata))
});
```

Do not change the formatting layer, global `EnvFilter`, or trace layer.

- [ ] **Step 4: Run focused policy tests and verify GREEN**

Run:

```bash
cargo test telemetry::tests::remote_log_filter_rejects_only_the_local_only_application_target --lib -- --exact
cargo test telemetry::tests::remote_layers_accept_only_application_targets --lib -- --exact
```

Expected: both tests pass.

- [ ] **Step 5: Format and commit the policy**

Run:

```bash
cargo fmt --all
cargo fmt --all -- --check
git add src/telemetry.rs
git commit -m "feat: add local-only telemetry log target"
```

Expected: formatting passes and the commit contains only the target policy, filter wiring, and focused test.

---

### Task 2: Demote and verify the webhook completion event

**Files:**
- Modify: `src/api/webhook.rs:15,25-31,301-305`
- Modify: `src/telemetry/otlp_test.rs:713-755,803-810,1224-1232`
- Create: `changelog/2026-08-11T10-11-33-0400-local-only-webhook-completion-logs.md`
- Test: `src/telemetry/otlp_test.rs`

**Interfaces:**
- Consumes: `crate::telemetry::LOCAL_ONLY_LOG_TARGET` from Task 1 and the existing `WebhookTraceFixture`, `CapturedSpans::has_log_body`, and `CapturedOutput` test infrastructure.
- Produces: local DEBUG-only generic webhook completion events with unchanged `result` fields and a real-pipeline regression test.

- [ ] **Step 1: Make the webhook OTLP fixture support DEBUG logging**

Add a debug constructor and pass a `rust_log` parameter into the existing shared constructor:

```rust
async fn new() -> Self {
    Self::new_with_exporter_timeout_and_step_limit(
        2_000,
        DEFAULT_WORKFLOW_JOB_MAX_STEPS,
        false,
        "github_webhook_exporter=info",
    )
    .await
}

async fn new_with_debug_logging() -> Self {
    Self::new_with_exporter_timeout_and_step_limit(
        2_000,
        DEFAULT_WORKFLOW_JOB_MAX_STEPS,
        false,
        "github_webhook_exporter=debug",
    )
    .await
}
```

Add `rust_log: &str` as the fourth parameter to
`new_with_exporter_timeout_and_step_limit`, update the other two callers to pass
`"github_webhook_exporter=info"`, and replace the constructor's hard-coded filter with:

```rust
let (runtime, subscriber) =
    build_runtime(rust_log, &config, output.clone(), metrics.clone())
        .expect("telemetry runtime initializes");
```

This is test-only setup and does not alter production behavior.

- [ ] **Step 2: Write the failing end-to-end regression test**

Add this focused integration test near the existing webhook telemetry tests:

```rust
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn webhook_completion_is_local_debug_only() {
    let fixture = WebhookTraceFixture::new_with_debug_logging().await;
    let delivery_id = "550e8400-e29b-41d4-a716-446655440200";
    let body = format!(
        r#"{{"repository":{{"full_name":"{WEBHOOK_REPOSITORY}"}}}}"#
    );

    let response = fixture
        .webhook(body.as_bytes(), "ping", delivery_id, WEBHOOK_SECRET)
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    drop(response);

    let (captured, stderr) = fixture.finish();
    assert!(stderr.contains("GitHub webhook request processed"));
    assert!(stderr.contains("DEBUG"));
    assert!(!captured.has_log_body("GitHub webhook request processed"));
}
```

The test uses the real subscriber, webhook middleware, OTLP bridge, queue, exporter, and protobuf receiver. The production change that makes it pass is changing the event level and target; do not weaken any assertion.

- [ ] **Step 3: Run the regression test and verify RED**

Run:

```bash
cargo test telemetry::otlp_test::webhook_completion_is_local_debug_only --lib -- --exact
```

Expected: FAIL because the existing event is INFO and is present in captured OTLP logs; the local output also does not identify it as DEBUG.

- [ ] **Step 4: Demote the event and assign the local-only target**

In `src/api/webhook.rs`, replace the unused `info` import with `debug`, import
`LOCAL_ONLY_LOG_TARGET` beside the existing telemetry trace imports, and change only the generic
completion event:

```rust
debug!(
    target: LOCAL_ONLY_LOG_TARGET,
    parent: None,
    result = result.as_str(),
    "GitHub webhook request processed"
);
```

Keep `metrics.observe_request(...)`, `result_for_status(...)`, the message text, and the bounded
`result` field unchanged.

- [ ] **Step 5: Run focused tests and verify GREEN**

Run:

```bash
cargo test telemetry::otlp_test::webhook_completion_is_local_debug_only --lib -- --exact
cargo test telemetry::tests::remote_log_filter_rejects_only_the_local_only_application_target --lib -- --exact
cargo test api::webhook::tests --lib
```

Expected: all focused tests pass; local output contains the DEBUG event and captured OTLP logs do not.

- [ ] **Step 6: Add the required changelog entry**

Create `changelog/2026-08-11T10-11-33-0400-local-only-webhook-completion-logs.md` with:

```markdown
# Local-only webhook completion logs

Changed the generic `GitHub webhook request processed` event from INFO to DEBUG and assigned it an
explicit local-only tracing target. Operators can enable the event through `RUST_LOG`, while the
OpenTelemetry log layer now rejects that target without suppressing other application DEBUG logs.

Added unit coverage for the target admission policy and an end-to-end webhook regression proving
the event remains visible locally but absent from exported OTLP protobuf logs.

## Validation

- `cargo test telemetry::otlp_test::webhook_completion_is_local_debug_only --lib -- --exact`
- `cargo test --all-targets`
- `cargo build --all-targets`
- `cargo clippy --all-targets -- -D warnings`
- `cargo fmt --all -- --check`
```

- [ ] **Step 7: Run full project verification**

Run:

```bash
cargo test --all-targets
cargo build --all-targets
cargo clippy --all-targets -- -D warnings
cargo fmt --all -- --check
git diff --check
```

Expected: every command exits successfully with no warnings or formatting errors.

- [ ] **Step 8: Commit the completed behavior**

Run:

```bash
git add src/api/webhook.rs src/telemetry/otlp_test.rs \
  changelog/2026-08-11T10-11-33-0400-local-only-webhook-completion-logs.md
git commit -m "fix: keep webhook completion logs local"
git status --short
```

Expected: the commit succeeds and the worktree is clean.
