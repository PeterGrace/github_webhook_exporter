# Completed Workflow Job OTLP Traces Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Export each newly claimed `workflow_job.completed` delivery as one independent historical OTLP job trace with a child span for every reported step.

**Architecture:** A focused webhook projection converts authenticated GitHub JSON into validated, bounded workflow telemetry values after the durable claim boundary. A cloneable historical emitter backed by the existing `SdkTracerProvider` builds explicit-time OpenTelemetry roots and children and submits them through the existing bounded, non-blocking trace processor.

**Tech Stack:** Rust 2021, Axum 0.8, Serde/serde_json, time 0.3, tracing, OpenTelemetry/OpenTelemetry SDK 0.32, in-process OTLP protobuf receiver tests.

## Global Constraints

- Process only authenticated `workflow_job.completed` events after `DeliveryClaim::New`.
- Emit fixed operation names `github.workflow.job` and `github.workflow.step`.
- Emit one independent root per completed job and one direct child per reported step.
- Sanitize names by removing Unicode control characters and retaining at most 128 characters.
- Retain no more than the first 20 positive pull-request numbers.
- Normalize conclusions to `success`, `failure`, `cancelled`, `skipped`, `timed_out`, `neutral`, or `other`; never export raw unknown values.
- Set OpenTelemetry error status only for `failure` and `timed_out`, OK only for `success`, and leave status unset for all other conclusions.
- Use reported timing only for valid ordered timestamps bounded by the selected parent interval; otherwise use an instantaneous fallback at job completion or request receipt.
- Never persist workflow payloads, workflow identifiers, or correlation state.
- Never export commands, output, logs, actors, URLs, bodies, signatures, secrets, authorization/OTLP headers, or raw unrecognized conclusions.
- Duplicate delivery IDs must not emit duplicate workflow traces.
- Collector availability must not alter webhook responses, readiness, metrics, or durable merge-queue state.

## File Structure

- Create `src/telemetry/workflow.rs`: bounded workflow trace model, conclusion/name policy, explicit-time SDK emitter, and focused unit tests.
- Create `src/api/workflow_job.rs`: minimal JSON projection, positive identifier validation, timestamp selection, and projection unit tests.
- Modify `src/telemetry.rs`: register the workflow module, construct the historical emitter from the existing SDK tracer, and expose a cloneable handle from `TelemetryRuntime`.
- Modify `src/telemetry/trace.rs`: centralize workflow attribute keys and shared identifier attribute builders without using `tracing` fields.
- Modify `src/api/mod.rs`: register the workflow-job module.
- Modify `src/api/webhook.rs`: invoke specialized projection/emission only after a new delivery claim.
- Modify `src/app.rs`: store the optional historical emitter in immutable application state.
- Modify `src/main.rs`: wire the runtime's emitter into production `AppState`.
- Modify `src/telemetry/otlp_test.rs`: extend capture helpers/allowlists and add full OTLP hierarchy, timing, status, privacy, deduplication, and unavailability tests.
- Modify `docs/operations.md`: document completed workflow traces and their privacy/timing behavior.
- Create `changelog/2026-08-06T17-17-20-0400-workflow-job-otlp-traces.md`: record the implementation and validation evidence.

---

### Task 1: Bounded Workflow Telemetry Model

**Files:**
- Create: `src/telemetry/workflow.rs`
- Modify: `src/telemetry.rs`
- Modify: `src/telemetry/trace.rs`

**Interfaces:**
- Produces: `WorkflowRunId::new(i64) -> Result<WorkflowRunId, WorkflowValueError>`
- Produces: `WorkflowRunAttempt::new(i64) -> Result<WorkflowRunAttempt, WorkflowValueError>`
- Produces: `WorkflowJobId::new(i64) -> Result<WorkflowJobId, WorkflowValueError>`
- Produces: `DisplayName::sanitize(&str) -> Option<DisplayName>`
- Produces: `WorkflowConclusion::normalize(Option<&str>) -> WorkflowConclusion`
- Produces: `HistoricalTiming { start: SystemTime, end: SystemTime, source: TimingSource }`
- Produces: owned `WorkflowJobTrace` and `WorkflowStepTrace` values consumed by the emitter and constructed by the API projection.

- [ ] **Step 1: Write failing policy tests**

Add `mod workflow;` to `src/telemetry.rs`, create `src/telemetry/workflow.rs`, and add tests named:

```rust
#[test]
fn positive_workflow_identifiers_reject_zero_and_negative_values() {
    assert!(WorkflowRunId::new(1).is_ok());
    assert!(WorkflowRunAttempt::new(1).is_ok());
    assert!(WorkflowJobId::new(1).is_ok());
    assert!(WorkflowRunId::new(0).is_err());
    assert!(WorkflowRunAttempt::new(-1).is_err());
    assert!(WorkflowJobId::new(0).is_err());
}

#[test]
fn display_names_remove_controls_and_stop_after_128_characters() {
    let input = format!("alpha\n{}omega", "x".repeat(200));
    let name = DisplayName::sanitize(&input).expect("visible characters remain");
    assert_eq!(name.as_str().chars().count(), 128);
    assert!(!name.as_str().chars().any(char::is_control));
    assert_eq!(DisplayName::sanitize("\n\r\t"), None);
}

#[test]
fn conclusions_have_a_closed_normalized_vocabulary() {
    let cases = [
        (Some("success"), WorkflowConclusion::Success, Some("success")),
        (Some("failure"), WorkflowConclusion::Failure, Some("failure")),
        (Some("cancelled"), WorkflowConclusion::Cancelled, Some("cancellation")),
        (Some("skipped"), WorkflowConclusion::Skipped, Some("skip")),
        (Some("timed_out"), WorkflowConclusion::TimedOut, Some("timeout")),
        (Some("neutral"), WorkflowConclusion::Neutral, None),
        (Some("private-unknown"), WorkflowConclusion::Other, None),
        (None, WorkflowConclusion::Other, None),
    ];
    for (raw, expected, semantic_result) in cases {
        let conclusion = WorkflowConclusion::normalize(raw);
        assert_eq!(conclusion, expected);
        assert_eq!(conclusion.semantic_result(), semantic_result);
    }
}
```

Name the production changes that make these tests pass: checked positive newtypes, character-based sanitization, and an exhaustive conclusion enum.

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```bash
cargo test telemetry::workflow::tests --lib
```

Expected: compilation fails because the model types and methods referenced by the tests do not yet exist.

- [ ] **Step 3: Implement the minimal bounded model**

Implement private fields with crate-visible constructors/accessors, redacted `Debug` for display names and identifiers where appropriate, and these exact enums:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkflowConclusion {
    Success,
    Failure,
    Cancelled,
    Skipped,
    TimedOut,
    Neutral,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimingSource {
    Reported,
    Fallback,
}
```

`WorkflowConclusion::as_str()` returns the seven issue-mandated GitHub values.
`semantic_result()` maps only success/failure/cancelled/skipped/timed-out to the CI/CD convention.
`status()` returns `Status::Ok`, fixed `Status::error("workflow_failed")`, or `Status::Unset`
according to the global constraints. Use a small private macro to define the three positive `i64`
newtypes without duplicating validation logic, but emit distinct concrete types.

Define owned data passed across the API/telemetry boundary:

```rust
pub(crate) struct WorkflowJobTrace {
    pub(crate) repository_name: CanonicalRepositoryName,
    pub(crate) delivery_id: DeliveryId,
    pub(crate) workflow_name: Option<DisplayName>,
    pub(crate) run_id: WorkflowRunId,
    pub(crate) run_attempt: WorkflowRunAttempt,
    pub(crate) job_id: WorkflowJobId,
    pub(crate) job_name: Option<DisplayName>,
    pub(crate) conclusion: WorkflowConclusion,
    pub(crate) head_sha: Option<CommitSha>,
    pub(crate) pull_requests: Vec<PullRequestNumber>,
    pub(crate) timing: HistoricalTiming,
    pub(crate) steps: Vec<WorkflowStepTrace>,
}

pub(crate) struct WorkflowStepTrace {
    pub(crate) number: i64,
    pub(crate) name: Option<DisplayName>,
    pub(crate) conclusion: WorkflowConclusion,
    pub(crate) timing: HistoricalTiming,
}
```

`WorkflowStepTrace::new` rejects non-positive step numbers so malformed step objects make the
specialized projection unsupported rather than creating sentinel identifiers.

In `src/telemetry/trace.rs`, centralize constants for all workflow keys. Keep identifiers out of
ordinary `tracing` fields. Add pure `KeyValue` builders where both ordinary and SDK spans need the
same validated value, and make existing setters delegate to those builders.

- [ ] **Step 4: Run the focused tests and verify GREEN**

Run:

```bash
cargo test telemetry::workflow::tests --lib
```

Expected: all workflow model tests pass with no warnings.

- [ ] **Step 5: Commit the model**

```bash
git add src/telemetry.rs src/telemetry/trace.rs src/telemetry/workflow.rs
git commit -m "feat: add bounded workflow trace model"
```

---

### Task 2: Authenticated Workflow-Job Projection and Timing

**Files:**
- Create: `src/api/workflow_job.rs`
- Modify: `src/api/mod.rs`

**Interfaces:**
- Consumes: the bounded model from Task 1.
- Produces: `project_completed_job(body, repository_name, delivery_id, received_at) -> Option<WorkflowJobTrace>`.
- Produces: no persistence, logging, metrics, or exporter side effects.

- [ ] **Step 1: Write failing projection tests**

Register `mod workflow_job;`, then add focused unit tests in the new module. Construct JSON with
`serde_json::json!` and assert:

```rust
#[test]
fn completed_projection_keeps_only_validated_bounded_fields() {
    let trace = project_fixture(json!({
        "workflow_job": {
            "id": 41,
            "run_id": 31,
            "run_attempt": 2,
            "workflow_name": "Build\nWorkflow",
            "name": "Linux\tJob",
            "conclusion": "success",
            "head_sha": "0123456789abcdef0123456789abcdef01234567",
            "started_at": "2026-08-06T10:00:00Z",
            "completed_at": "2026-08-06T10:05:00Z",
            "pull_requests": [{"number": 7}, {"number": -1}],
            "steps": [{
                "number": 1,
                "name": "Checkout\n",
                "conclusion": "success",
                "started_at": "2026-08-06T10:00:00Z",
                "completed_at": "2026-08-06T10:01:00Z"
            }]
        }
    })).expect("valid completed job projects");
    assert_eq!(trace.run_id.get(), 31);
    assert_eq!(trace.run_attempt.get(), 2);
    assert_eq!(trace.job_id.get(), 41);
    assert_eq!(trace.pull_requests.len(), 1);
    assert_eq!(trace.steps.len(), 1);
    assert_eq!(trace.timing.source, TimingSource::Reported);
    assert_eq!(trace.steps[0].timing.source, TimingSource::Reported);
}
```

Add separate tests proving:

- zero/negative required IDs reject the whole specialized projection;
- non-string/missing/malformed timestamps select fallback;
- reversed job timestamps fall back at valid completion;
- reversed or out-of-parent step timestamps fall back at the selected job end;
- exactly 20 of 25 positive PR numbers are retained;
- every valid step object is retained in input order;
- commands, output, logs, actor, and URL fields have no representation in the output model; and
- malformed/non-array `steps` rejects the specialized projection without exposing serde details.

- [ ] **Step 2: Run the projection tests and verify RED**

Run:

```bash
cargo test api::workflow_job::tests --lib
```

Expected: compilation fails because `project_completed_job` and its private Serde projections do not exist.

- [ ] **Step 3: Implement the minimal projection**

Deserialize only a wrapper containing `workflow_job`. Use `serde_json::Value` for timestamps,
conclusions, names, and SHA fields that must degrade safely when GitHub supplies a wrong JSON type.
Use typed `i64` for required IDs and step numbers. Use a private PR projection containing only
`number: i64`.

Implement these helpers with single responsibilities:

```rust
fn parse_timestamp(value: Option<&Value>) -> Option<SystemTime>;
fn select_job_timing(start: Option<SystemTime>, end: Option<SystemTime>, received_at: SystemTime)
    -> HistoricalTiming;
fn select_step_timing(start: Option<SystemTime>, end: Option<SystemTime>, parent: HistoricalTiming)
    -> HistoricalTiming;
fn positive_pull_requests(values: &[PullRequestProjection]) -> Vec<PullRequestNumber>;
```

Convert `OffsetDateTime` to `SystemTime` with checked addition/subtraction around `UNIX_EPOCH` so
pre-epoch or unrepresentable input cannot panic. Preallocate the step vector and PR vector with
bounded capacities. Do not clone or retain the request body.

- [ ] **Step 4: Run projection tests and verify GREEN**

Run:

```bash
cargo test api::workflow_job::tests --lib
```

Expected: all projection and timing tests pass.

- [ ] **Step 5: Commit the projection**

```bash
git add src/api/mod.rs src/api/workflow_job.rs
git commit -m "feat: project completed workflow jobs safely"
```

---

### Task 3: Explicit-Time Historical Trace Emitter and Runtime Wiring

**Files:**
- Modify: `src/telemetry/workflow.rs`
- Modify: `src/telemetry.rs`
- Modify: `src/app.rs`
- Modify: `src/main.rs`
- Modify: `src/telemetry/otlp_test.rs`

**Interfaces:**
- Consumes: `WorkflowJobTrace` from Tasks 1-2.
- Produces: `WorkflowTraceEmitter::disabled()` and `WorkflowTraceEmitter::emit(&WorkflowJobTrace)`.
- Produces: `TelemetryRuntime::workflow_trace_emitter() -> WorkflowTraceEmitter`.
- Produces: `AppState::with_workflow_trace_emitter(WorkflowTraceEmitter) -> AppState` and `AppState::workflow_trace_emitter(&self) -> &WorkflowTraceEmitter`.

- [ ] **Step 1: Write a failing explicit-time emitter test**

Use `SdkTracerProvider` with `SimpleSpanProcessor` and the existing collecting test exporter pattern.
Build one `WorkflowJobTrace` with two steps and fixed `SystemTime` boundaries. Assert after force
flush:

```rust
assert_eq!(spans.iter().filter(|span| span.name == "github.workflow.job").count(), 1);
assert_eq!(spans.iter().filter(|span| span.name == "github.workflow.step").count(), 2);
assert_eq!(job.parent_span_id, SpanId::INVALID);
assert!(steps.iter().all(|step| step.parent_span_id == job.span_context.span_id()));
assert_eq!(job.start_time, job_timing.start);
assert_eq!(job.end_time, job_timing.end);
assert_eq!(steps[0].start_time, first_step_timing.start);
assert_eq!(steps[0].end_time, first_step_timing.end);
```

Also assert the root and children contain only the approved CI/CD/GitHub attributes, the root has
repository/delivery/run/attempt/job/SHA/PR identifiers, and children do not inherit identifier
attributes by duplication.

- [ ] **Step 2: Run the emitter test and verify RED**

Run:

```bash
cargo test telemetry::workflow::tests::emitter_exports_independent_historical_job_and_step_spans --lib
```

Expected: compilation fails because `WorkflowTraceEmitter` does not yet exist.

- [ ] **Step 3: Implement the emitter minimally**

Define:

```rust
#[derive(Clone, Debug, Default)]
pub(crate) struct WorkflowTraceEmitter {
    tracer: Option<SdkTracer>,
}
```

`disabled()` stores `None`; `new(SdkTracer)` stores `Some`. `emit` returns immediately when disabled.
For enabled emission:

1. Build the root with `tracer.span_builder("github.workflow.job")`, explicit start time, approved
   attributes, and normalized status.
2. Call `build_with_context(builder, &Context::new())` so the root ignores the active request.
3. Create `Context::current_with_span(root)` only as an owned parent context, without attaching it to
   the thread or async task.
4. Build each child against that context, then call `end_with_timestamp(step.timing.end)`.
5. End the root through `parent_context.span().end_with_timestamp(job.timing.end)` after all children.

Use decimal strings for semantic-convention IDs. Build each step task-run ID with
`format!("{}:{}", job_id.get(), step.number)` so it is unique within the workflow run. Reserve
attribute vector capacity exactly from the optional field count; do not create logs or tracing
spans while emitting.

- [ ] **Step 4: Wire the emitter through runtime and state**

When building the trace provider, construct one `SdkTracer` for both the tracing layer and historical
emitter rather than using a global provider. Store the emitter in `TelemetryRuntime`; disabled
runtime returns `WorkflowTraceEmitter::disabled()`.

Keep the existing `AppState::new` signature unchanged to avoid broad test churn. Initialize a
disabled emitter there, add the builder-style setter and accessor, and update production startup:

```rust
let telemetry_runtime = telemetry::init(config.rust_log(), config.telemetry())
    .context("failed to initialize telemetry")?;
let state = AppState::new(
    RepositoryStore::new(pool, cipher),
    AdminAuthenticator::new(config.admin_token()),
    config.webhook_body_limit_bytes(),
)
.with_workflow_trace_emitter(telemetry_runtime.workflow_trace_emitter());
```

Update `WebhookTraceFixture::new` the same way so integration requests use the fixture provider.
Keep the runtime alive through shutdown as it is today.

- [ ] **Step 5: Run emitter and existing telemetry tests and verify GREEN**

Run:

```bash
cargo test telemetry::workflow::tests --lib
cargo test telemetry::otlp_test --lib
```

Expected: emitter tests and all pre-existing OTLP tests pass.

- [ ] **Step 6: Commit emitter wiring**

```bash
git add src/telemetry/workflow.rs src/telemetry.rs src/app.rs src/main.rs src/telemetry/otlp_test.rs
git commit -m "feat: emit explicit-time workflow traces"
```

---

### Task 4: Compose Emission After the Durable Claim Boundary

**Files:**
- Modify: `src/api/webhook.rs`
- Modify: `src/telemetry/otlp_test.rs`

**Interfaces:**
- Consumes: `project_completed_job` and the state emitter.
- Preserves: authentication, generic normalization, durable claim, metrics, response, and merge-queue processing order.

- [ ] **Step 1: Write a failing end-to-end hierarchy test**

Add `workflow_job_completed_exports_one_independent_historical_trace` to the OTLP test module. Send a
signed payload with a valid job, two steps, names containing controls, run/attempt/job/SHA IDs, and
PR references. Flush and assert:

```rust
let job = captured.one_named("github.workflow.job");
assert!(job.parent_span_id.is_empty());
assert_eq!(captured.child_count(job, "github.workflow.step"), 2);
assert_ne!(
    job.trace_id,
    captured.webhook_request_for_delivery(delivery_id).trace_id,
);
assert_attribute(job, "cicd.pipeline.name", "BuildWorkflow");
assert_attribute(job, "cicd.pipeline.task.name", "LinuxJob");
assert_attribute(job, "github.workflow.conclusion", "success");
```

Assert exact nanosecond timestamps from the fixture payload, child ordering by step number attribute,
all approved identifiers, the 20-PR cap, and response status `204`.

- [ ] **Step 2: Run the hierarchy test and verify RED**

Run:

```bash
cargo test telemetry::otlp_test::workflow_job_completed_exports_one_independent_historical_trace --lib -- --exact
```

Expected: the request returns `204`, but no `github.workflow.job` span exists because webhook dispatch is not wired.

- [ ] **Step 3: Add the minimal post-claim dispatch**

Inside only the `DeliveryClaim::New` arm, after generic metrics accounting, check the already
normalized pair:

```rust
if event_type == EventType::WorkflowJob && action == Action::Completed {
    if let Some(workflow_trace) = workflow_job::project_completed_job(
        request.body.as_ref(),
        &request.repository_name,
        request.delivery_id,
        received_at,
    ) {
        state.workflow_trace_emitter().emit(&workflow_trace);
    }
}
```

Use `OffsetDateTime` or `SystemTime` consistently at the boundary according to Task 2's final
signature. Do not emit an error, metric, or log for an unsupported specialized projection. Keep
merge-group and pull-request processing unchanged.

- [ ] **Step 4: Run the hierarchy test and verify GREEN**

Run:

```bash
cargo test telemetry::otlp_test::workflow_job_completed_exports_one_independent_historical_trace --lib -- --exact
```

Expected: one independent job root, two direct children, exact attributes/timestamps, and `204`.

- [ ] **Step 5: Commit composition**

```bash
git add src/api/webhook.rs src/telemetry/otlp_test.rs
git commit -m "feat: trace newly claimed completed workflow jobs"
```

---

### Task 5: Conclusion, Timing, Action, and Deduplication Coverage

**Files:**
- Modify: `src/telemetry/otlp_test.rs`
- Modify only if a RED test exposes a defect: `src/api/workflow_job.rs`, `src/telemetry/workflow.rs`, or `src/api/webhook.rs`

**Interfaces:**
- Verifies all issue-mandated normalized outcomes and timing paths through captured OTLP protobuf.

- [ ] **Step 1: Add failing conclusion/status matrix tests**

Submit completed jobs/steps covering `success`, `failure`, `cancelled`, `skipped`, `timed_out`,
`neutral`, and a fixture-only unknown raw conclusion. Assert exact normalized attributes, compatible
CI/CD result attributes, and protobuf status codes/descriptions:

- success: `STATUS_CODE_OK`;
- failure: `STATUS_CODE_ERROR` with only `workflow_failed`;
- timed out: `STATUS_CODE_ERROR` with only `workflow_failed`;
- cancelled/skipped/neutral/other: `STATUS_CODE_UNSET` and empty message.

Before writing each assertion, identify `WorkflowConclusion::status` or `semantic_result` as the
production method whose mutation would make it fail.

- [ ] **Step 2: Run conclusion tests and verify RED when behavior is missing**

Run:

```bash
cargo test telemetry::otlp_test::workflow_conclusions_export_bounded_results_and_statuses --lib -- --exact
```

Expected: at least one assertion fails until every conclusion is represented correctly by emitted protobuf.

- [ ] **Step 3: Make the conclusion matrix GREEN**

Adjust only the closed enum mappings or emitter attribute/status construction. Never pass raw
conclusion strings beyond `WorkflowConclusion::normalize`.

Run the same focused command and expect PASS.

- [ ] **Step 4: Add timing fallback and unsupported-input tests**

Add tests proving:

- malformed, missing, and reversed job timestamps produce an instantaneous root at valid completion
  or request receipt with `timing_source=fallback`;
- malformed, missing, reversed, and out-of-parent step timestamps produce instantaneous children at
  the job end with fallback source;
- valid ordered job/step timestamps preserve exact nanoseconds with reported source;
- `queued`, `in_progress`, absent, and unknown actions emit no workflow root;
- malformed required IDs and malformed `steps` emit no workflow root while returning `204`; and
- generic event/body metrics still increment for every newly claimed authenticated request.

- [ ] **Step 5: Run timing/input tests and verify RED, then GREEN**

Run each new exact test before changing production code. Confirm its failure is the missing behavior,
make the smallest correction in projection or dispatch, then rerun:

```bash
cargo test telemetry::otlp_test::workflow_timing_uses_reported_and_bounded_fallback_intervals --lib -- --exact
cargo test telemetry::otlp_test::unsupported_workflow_actions_and_projections_emit_no_historical_trace --lib -- --exact
```

Expected after correction: both pass.

- [ ] **Step 6: Add and verify duplicate-delivery suppression**

Send the same completed payload twice under one delivery UUID. Assert both responses are `204`, one
durable claim exists, generic event metrics increment once, duplicate metrics increment once, and
exactly one job root plus its original children exist.

Run before correction if needed:

```bash
cargo test telemetry::otlp_test::duplicate_workflow_delivery_emits_one_historical_trace --lib -- --exact
```

Expected: PASS from placement inside `DeliveryClaim::New`; if it fails, move only the specialized
call rather than changing delivery storage semantics.

- [ ] **Step 7: Commit edge behavior**

```bash
git add src/telemetry/otlp_test.rs src/api/workflow_job.rs src/telemetry/workflow.rs src/api/webhook.rs
git commit -m "test: cover workflow trace outcomes and timing"
```

---

### Task 6: Privacy, Collector Failure, and Operational Documentation

**Files:**
- Modify: `src/telemetry/otlp_test.rs`
- Modify: `docs/operations.md`
- Create: `changelog/2026-08-06T17-17-20-0400-workflow-job-otlp-traces.md`

**Interfaces:**
- Verifies workflow names/identifiers are span-only and forbidden data is absent from every captured signal.
- Documents operator-visible behavior without exposing fixture secrets.

- [ ] **Step 1: Extend the centralized OTLP attribute allowlists**

Add the exact approved workflow keys used by the emitter to `SPAN_ATTRIBUTE_ALLOWLIST` and add
workflow IDs, PR number arrays, and sanitized names to the span-only boundary checks. Add capture
helpers for integer-array attributes and explicit start/end nanoseconds instead of manually decoding
attributes in each test.

Run:

```bash
cargo test telemetry::otlp_test --lib
```

Expected before allowlist updates: privacy allowlist assertions fail on each newly approved workflow key.

- [ ] **Step 2: Add a failing cross-signal privacy test**

Create a payload containing unique fixture values in workflow/job/step names and forbidden command,
logs, actor, URL, secret-like, signature-like, header-like, and unknown-conclusion fields. Assert:

- sanitized names and approved IDs exist in spans;
- those names and IDs are absent from OTLP logs, structured stderr, and Prometheus exposition;
- every forbidden fixture value is absent from serialized traces, serialized logs, stderr, and
  exposition; and
- no unapproved span attribute/event key exists.

Run:

```bash
cargo test telemetry::otlp_test::workflow_identifiers_and_names_are_span_only_and_payload_data_is_absent --lib -- --exact
```

Expected: fail until all allowlists/helpers and emitter attributes enforce the complete boundary.

- [ ] **Step 3: Correct privacy failures minimally and verify GREEN**

Remove any unsafe projection field or emitter attribute found by the test. Do not redact after
export; prevent forbidden data from entering the model. Rerun the exact privacy test and the entire
OTLP test module.

- [ ] **Step 4: Add collector-unavailability coverage**

Configure a trace endpoint that refuses connections with the existing short timeout fixture. Send a
valid completed workflow job and assert `204`, unchanged readiness, expected generic metrics, and no
merge-queue rows. Force-flush may report exporter failure through the existing runtime counter, but
the request path must complete without waiting for the collector.

Run:

```bash
cargo test telemetry::otlp_test::unavailable_collector_does_not_change_completed_workflow_response --lib -- --exact
```

Expected: PASS without adding retries, awaits, logs containing exporter details, or response changes.

- [ ] **Step 5: Update operator documentation**

Add a `Completed workflow traces` subsection to `docs/operations.md` documenting:

- completed-only and at-most-once admission;
- fixed root/child names and independent trace identity;
- reported/fallback timing rules;
- conclusion/status mappings;
- the approved span-only identifiers and 20-PR limit;
- name sanitization and 128-character cap; and
- forbidden payload fields plus collector-failure isolation.

- [ ] **Step 6: Add the implementation changelog**

Create `changelog/2026-08-06T17-17-20-0400-workflow-job-otlp-traces.md`. Record the projection,
historical emitter, privacy policy, integration coverage, and exact final validation commands. Do
not claim a command passed until Task 7 has produced evidence; update the file with results there.

- [ ] **Step 7: Commit privacy and documentation**

```bash
git add src/telemetry/otlp_test.rs docs/operations.md changelog/
git commit -m "docs: describe completed workflow traces"
```

---

### Task 7: Full Verification and Review Readiness

**Files:**
- Modify if verification requires a fix: only files already listed by Tasks 1-6.
- Modify: the Task 6 timestamped changelog with final evidence.

**Interfaces:**
- Produces a warning-free, documented branch ready for review and PR creation.

- [ ] **Step 1: Format**

Run:

```bash
just fmt
```

Expected: `cargo fmt --all -- --check` succeeds. If it fails, run `cargo fmt --all`, inspect the
format-only diff, then restart the full gate sequence from this step.

- [ ] **Step 2: Build**

Run:

```bash
cargo build
```

Expected: success with no warnings.

- [ ] **Step 3: Lint all targets**

Run:

```bash
cargo clippy --all-targets -- -D warnings
```

Expected: success with no warnings. Correct any warning without weakening lints, then restart from
`just fmt`.

- [ ] **Step 4: Run every test target**

Run:

```bash
just test
```

Expected: all library, binary, and integration tests pass. For a failure, follow systematic
debugging, add a focused regression when production behavior is wrong, then restart from `just fmt`.

- [ ] **Step 5: Build public documentation**

Run:

```bash
cargo doc --no-deps
```

Expected: documentation builds without warnings.

- [ ] **Step 6: Review the artifact-specific evidence**

Confirm the focused OTLP tests executed under `just test` include hierarchy, exact timestamps,
conclusion/status matrix, fallback timing, malformed/unsupported input, duplicate delivery,
collector unavailability, and cross-signal privacy. Record all five gate results in the timestamped
changelog.

- [ ] **Step 7: Inspect the final diff**

Run:

```bash
git diff origin/main...HEAD --check
git status --short
git log --oneline origin/main..HEAD
```

Expected: no whitespace errors, only intentional files, and a clean working tree after the final
commit.

- [ ] **Step 8: Commit any final verification-only correction**

If Task 7 changed source, tests, docs, or changelog, stage only those reviewed files and commit:

```bash
git add src tests docs changelog
git commit -m "test: finalize workflow trace validation"
```

If no files changed, do not create an empty commit.
