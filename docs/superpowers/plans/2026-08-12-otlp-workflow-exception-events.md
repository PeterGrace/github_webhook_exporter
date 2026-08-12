# OTLP Workflow Exception Events Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Export a canonical OpenTelemetry `exception` event on every failed or timed-out workflow task span while retaining optional Sentry Issue promotion.

**Architecture:** `SyntheticWorkflowError` remains the single bounded failure model and supplies the semantic exception event attributes. `WorkflowTraceEmitter` always records that event on the exact historical span, then independently invokes the optional Sentry reporter with the same value. Existing child-failure suppression determines whether the job receives a fallback event.

**Tech Stack:** Rust, OpenTelemetry Rust SDK 0.32, OTLP protobuf, Sentry Rust SDK 0.49, built-in Rust tests.

## Global Constraints

- The canonical event name is `exception` and it is exported through the existing OTLP traces endpoint.
- Event attributes are limited to `exception.type` and `exception.message`.
- Event timestamps equal the task's selected historical end time.
- Failed and timed-out steps each receive one event; a failed/timed-out job receives one fallback only when no failed/timed-out child exists.
- `SENTRY_DSN` adds Sentry reporting and never suppresses or duplicates the canonical OTLP event.
- No CI logs, commands, output, payloads, arbitrary GitHub error text, stack traces, or secrets may be exported.
- Existing OTLP status and Sentry trace/span linkage remain unchanged.

---

### Task 1: Record canonical exception events on historical spans

**Files:**
- Modify: `src/telemetry/workflow.rs:480-570,1180-1360`
- Modify: `src/telemetry/workflow_error.rs:100-245`
- Modify: `src/telemetry/otlp_test.rs:130-180,3638-3735`

**Interfaces:**
- Consumes: `SyntheticWorkflowError::{for_step,for_job,exception_type,description,timestamp}`.
- Produces: `SyntheticWorkflowError::span_event_attributes(&self) -> [KeyValue; 2]` and unconditional OTLP exception events in `WorkflowTraceEmitter::emit`.

- [ ] **Step 1: Write failing span-export tests**

Extend the in-memory exporter tests in `src/telemetry/workflow.rs` to inspect `SpanData.events`. Add literal assertions that a failure step and timeout step each carry exactly one event named `exception`, with `exception.type` and `exception.message`, while the job has no duplicate event. Add a separate case proving a failed job with only successful/skipped children has exactly one job event. Assert the event timestamps equal the corresponding historical end times. Extend the non-failure test to prove success, cancellation, skip, neutral, and other spans have no exception events.

Add `("exception", &["exception.type", "exception.message"])` to `SPAN_EVENT_ALLOWLIST` in `src/telemetry/otlp_test.rs`. Extend `workflow_conclusions_export_bounded_results_and_statuses` with named job and step fixtures and hand-derived assertions for the serialized event name, exact end timestamp, fixed type, and bounded message. Assert the job has no event when its failing child explains the failure. Add one completed failed-job fixture with only a successful child and assert the job fallback event exists while the child has none.

The production mutation caught by these tests is removing, misplacing, duplicating, mistiming, dropping, or malformed serialization of canonical task exception events while Sentry reporting still works.

- [ ] **Step 2: Run focused tests and verify RED**

Run:

```bash
cargo test telemetry::workflow::tests::emitter_exports_otlp_exception_events -- --exact
cargo test telemetry::workflow::tests::emitter_exports_job_exception_only_as_fallback -- --exact
cargo test telemetry::otlp_test::workflow_conclusions_export_bounded_results_and_statuses -- --exact
```

Expected: FAIL because emitted spans contain no `exception` events.

- [ ] **Step 3: Add the minimal shared event representation**

In `src/telemetry/workflow_error.rs`, import `opentelemetry::KeyValue` and add:

```rust
pub(super) fn span_event_attributes(&self) -> [KeyValue; 2] {
    [
        KeyValue::new("exception.type", self.exception_type),
        KeyValue::new("exception.message", self.description.clone()),
    ]
}
```

The fixed type is borrowed without allocation; the bounded description is cloned once because OpenTelemetry event attributes own their values.

- [ ] **Step 4: Record the event before optional Sentry reporting**

For each failed/timed-out step, construct `SyntheticWorkflowError` once, call:

```rust
span.add_event_with_timestamp(
    "exception",
    error.span_event_attributes(),
    error.timestamp(),
);
```

Then call the optional reporter with that same value. For the job fallback, recover the root span mutably from `parent_context.span()`, record the same event, optionally report to Sentry, then end it. Keep `child_error_emitted` independent of whether a Sentry reporter exists.

- [ ] **Step 5: Run focused tests and verify GREEN**

Run:

```bash
cargo test telemetry::workflow::tests::emitter_ -- --nocapture
cargo test telemetry::workflow_error::tests:: -- --nocapture
```

Expected: all selected tests PASS.

- [ ] **Step 6: Commit the behavior**

```bash
git add src/telemetry/workflow.rs src/telemetry/workflow_error.rs src/telemetry/otlp_test.rs
git commit -m "feat: export OTLP workflow exception events"
```

---

### Task 2: Document the vendor-neutral baseline

**Files:**
- Modify: `book/src/reference/traces.md:80-105`
- Modify: `book/src/reference/telemetry.md:8-18`
- Modify: `book/src/how-to/configure-remote-telemetry.md`
- Modify: `changelog/2026-08-12T16-48-49Z-linked-sentry-workflow-errors.md`
- Create: `changelog/2026-08-12T19-10-42Z-otlp-workflow-exception-events.md`

**Interfaces:**
- Consumes: serialized OTLP `Span.events` emitted and tested by Task 1.
- Produces: operator documentation that distinguishes canonical OTLP events from optional Sentry promotion.

- [ ] **Step 1: Update operator documentation**

Document that OTLP trace export always includes bounded `exception` events for failed/timed-out tasks. State that `SENTRY_DSN` additionally emits a Sentry envelope for native Issue grouping and does not disable the OTLP event. State explicitly that there is no OTLP errors endpoint and no Sentry configuration is required for the canonical representation.

Add a timestamped changelog entry and update the existing PR changelog so the final release narrative describes both paths without implying Sentry is required.

- [ ] **Step 2: Run focused privacy and telemetry tests**

Run:

```bash
cargo test telemetry::otlp_test::workflow_ -- --nocapture
cargo test telemetry::workflow::tests:: -- --nocapture
cargo test telemetry::workflow_error::tests:: -- --nocapture
```

Expected: all selected tests PASS, including serialized allowlist/privacy checks.

- [ ] **Step 3: Commit integration coverage and docs**

```bash
git add book/src/reference/traces.md book/src/reference/telemetry.md \
  book/src/how-to/configure-remote-telemetry.md \
  changelog/2026-08-12T16-48-49Z-linked-sentry-workflow-errors.md \
  changelog/2026-08-12T19-10-42Z-otlp-workflow-exception-events.md
git commit -m "docs: explain vendor-neutral workflow exceptions"
```

---

### Task 3: Validate and update PR #80

**Files:**
- Verify all modified files.
- Update: GitHub PR #80 description.

**Interfaces:**
- Consumes: Tasks 1 and 2.
- Produces: a clean, pushed PR with current validation evidence and an accurate manual test plan.

- [ ] **Step 1: Run all mandatory validation**

```bash
cargo fmt --all
just fmt
cargo clippy --all-targets -- -D warnings
just test
just helm-static
cargo doc --no-deps
git diff --check
```

Expected: every command exits zero; all tests report zero failures.

- [ ] **Step 2: Inspect the final branch**

```bash
git status --short
git log --oneline origin/main..HEAD
git diff --stat origin/main...HEAD
```

Expected: no uncommitted files and the diff contains only issue #78, its OTLP extension, tests, configuration, and documentation.

- [ ] **Step 3: Push and update PR #80**

```bash
git push
```

Edit PR #80 to explain that canonical OTLP exception events work without Sentry, while `SENTRY_DSN` additionally promotes the same bounded failure to a linked Sentry Issue. Preserve the note that a live Sentry waterfall check requires project credentials.
