# Workflow Job Step Limit Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bound completed workflow-job traces by a configurable 256-step default, expose job-size metrics, and emit an actionable warning for every over-limit job.

**Architecture:** A minimal first-pass Serde projection validates job identifiers and counts `steps` without retaining them. The webhook handler records the count, rejects over-limit jobs atomically with bounded metrics and an identifier-bearing warning, and invokes the existing detailed projection only for accepted jobs.

**Tech Stack:** Rust 2021, Axum, Serde/serde_json, `prometheus-client`, `tracing`, OpenTelemetry OTLP, Tokio, Cargo.

## Global Constraints

- `GHE_WORKFLOW_JOB_MAX_STEPS` defaults to `256`, accepts `1..=1024`, and has no unlimited mode.
- Apply the limit only to authenticated, newly claimed `workflow_job.completed` deliveries.
- Observe one step-count histogram value for each structurally valid admitted job, including over-limit jobs.
- Never truncate: accepted jobs emit every step; over-limit jobs emit no historical workflow spans.
- The `too_many_steps` warning may contain canonical repository name, run ID, run attempt, job ID, delivery UUID, observed count, and configured limit.
- Workflow/job/step names, commit SHA, URLs, actors, commands, logs, outputs, payload fragments, signatures, secrets, authorization headers, and collector details remain forbidden in rejection diagnostics.
- Repository and job identifiers must never become Prometheus labels.
- Preserve durable claiming, duplicate suppression, authenticated `204 No Content`, bounded non-blocking export, readiness, and merge-queue behavior.
- Write each production behavior test first and observe the intended failure before implementation.
- Do not add dependencies.
- Add a timestamped Markdown entry under `changelog/` for the implementation iteration.
- Final validation must run `just fmt`, `cargo build`, `cargo clippy --all-targets -- -D warnings`, `just test`, and `cargo doc --no-deps`.

## File Structure

- `src/config.rs`: validate and expose the configured workflow-job step limit.
- `src/app.rs`: carry the immutable validated limit in `AppState`.
- `src/main.rs`: pass runtime configuration into application state.
- `src/api/workflow_job.rs`: implement allocation-bounded admission parsing and retain detailed projection.
- `src/metrics.rs`: own the step histogram and closed rejection-reason counter family.
- `src/api/webhook.rs`: orchestrate admission, metric recording, rejection warning, and accepted emission.
- `src/telemetry/otlp_test.rs`: prove end-to-end trace, metric, stderr, OTLP-log, privacy, and deduplication behavior.
- `tests/webhook_api.rs`, `tests/repository_api.rs`: update explicit `AppState` construction for the new required argument.
- `docs/operations.md`: document configuration, metrics, rejection semantics, and job lookup.
- `changelog/<timestamp>-workflow-job-step-limit.md`: record the production change.

---

### Task 1: Validate and propagate workflow-job step configuration

**Files:**
- Modify: `src/config.rs`
- Modify: `src/app.rs`
- Modify: `src/main.rs`
- Modify: `tests/webhook_api.rs`
- Modify: `tests/repository_api.rs`
- Modify: `src/telemetry/otlp_test.rs`

**Interfaces:**
- Produces: `pub const DEFAULT_WORKFLOW_JOB_MAX_STEPS: usize = 256`
- Produces: private `MAX_WORKFLOW_JOB_MAX_STEPS: usize = 1_024`
- Produces: `RuntimeConfig::workflow_job_max_steps(&self) -> usize`
- Changes: `AppState::new(repository_store, admin_authenticator, webhook_body_limit_bytes, workflow_job_max_steps) -> Self`
- Produces: `AppState::workflow_job_max_steps(&self) -> usize`

- [ ] **Step 1: Add failing configuration tests**

Extend `valid_required_variables_use_documented_defaults` and `valid_overrides_replace_defaults` in
`src/config.rs`:

```rust
assert_eq!(config.workflow_job_max_steps(), 256);
```

Add `GHE_WORKFLOW_JOB_MAX_STEPS=1024` to the override fixture and assert:

```rust
assert_eq!(config.workflow_job_max_steps(), 1_024);
```

Add these cases to `invalid_values_report_only_variable_names`:

```rust
("GHE_WORKFLOW_JOB_MAX_STEPS", "0"),
("GHE_WORKFLOW_JOB_MAX_STEPS", "not-a-number"),
("GHE_WORKFLOW_JOB_MAX_STEPS", "1025"),
("GHE_WORKFLOW_JOB_MAX_STEPS", "18446744073709551616"),
```

Add `GHE_WORKFLOW_JOB_MAX_STEPS` to the Unix non-Unicode configuration test or create a focused
non-Unicode test with the same redaction assertions.

- [ ] **Step 2: Run configuration tests and verify RED**

Run:

```bash
cargo test config::tests --lib
```

Expected: compilation fails because `workflow_job_max_steps` does not exist, proving the tests
require the new configuration behavior.

- [ ] **Step 3: Implement validated runtime configuration**

In `src/config.rs`, add:

```rust
pub const DEFAULT_WORKFLOW_JOB_MAX_STEPS: usize = 256;
const MAX_WORKFLOW_JOB_MAX_STEPS: usize = 1_024;
```

Add `workflow_job_max_steps: usize` to `RuntimeConfig`, expose it with a documented public getter,
and parse it in `from_lookup`:

```rust
let workflow_job_max_steps = optional_positive_usize(
    &mut lookup,
    "GHE_WORKFLOW_JOB_MAX_STEPS",
    DEFAULT_WORKFLOW_JOB_MAX_STEPS,
)?;
if workflow_job_max_steps > MAX_WORKFLOW_JOB_MAX_STEPS {
    return Err(ConfigError::Invalid {
        variable: "GHE_WORKFLOW_JOB_MAX_STEPS",
    });
}
```

Include the value in `RuntimeConfig` construction and its redacted `Debug` output. Do not place it
inside `TelemetryConfig`; it governs authenticated event processing even when OTLP is disabled.

- [ ] **Step 4: Run configuration tests and verify GREEN**

Run:

```bash
cargo test config::tests --lib
```

Expected: all configuration tests pass.

- [ ] **Step 5: Add failing AppState propagation test**

Update the `app_state` fixture in `src/app.rs` to call the desired four-argument constructor with
`256`, then add:

```rust
#[tokio::test]
async fn application_state_exposes_workflow_job_step_limit() {
    let state = app_state().await;

    assert_eq!(state.workflow_job_max_steps(), 256);
}
```

- [ ] **Step 6: Run the AppState test and verify RED**

Run:

```bash
cargo test app::tests::application_state_exposes_workflow_job_step_limit --lib -- --exact
```

Expected: compilation fails because the constructor and getter do not yet carry the limit.

- [ ] **Step 7: Propagate the limit through application state**

Add `workflow_job_max_steps: usize` to `AppState`, require it in `AppState::new`, and add the
documented getter:

```rust
/// Returns the maximum reported steps accepted for one completed workflow-job trace.
pub fn workflow_job_max_steps(&self) -> usize {
    self.workflow_job_max_steps
}
```

In `src/main.rs`, pass `config.workflow_job_max_steps()` as the fourth constructor argument. Update
all nine `AppState::new` call sites in `src/app.rs`, `tests/webhook_api.rs`,
`tests/repository_api.rs`, and `src/telemetry/otlp_test.rs`; use
`DEFAULT_WORKFLOW_JOB_MAX_STEPS` unless a focused test needs another limit.

- [ ] **Step 8: Run focused propagation tests and verify GREEN**

Run:

```bash
cargo test config::tests --lib
cargo test app::tests::application_state_exposes_workflow_job_step_limit --lib -- --exact
cargo test --no-run
```

Expected: focused tests pass and every constructor call compiles.

- [ ] **Step 9: Commit Task 1**

```bash
git add src/config.rs src/app.rs src/main.rs tests/webhook_api.rs tests/repository_api.rs \
  src/telemetry/otlp_test.rs
git commit -m "feat: configure workflow job step limit"
```

---

### Task 2: Count workflow steps without retaining them

**Files:**
- Modify: `src/api/workflow_job.rs`

**Interfaces:**
- Consumes: `WorkflowRunId::new(i64)`, `WorkflowRunAttempt::new(i64)`, and
  `WorkflowJobId::new(i64)`
- Produces: `pub(super) struct WorkflowJobAdmission`
- Produces: `WorkflowJobAdmission::{run_id, run_attempt, job_id, step_count}` accessors
- Produces: `pub(super) fn inspect_completed_job(body: &[u8]) -> Option<WorkflowJobAdmission>`
- Preserves: `project_completed_job(...) -> Option<WorkflowJobTrace>`

- [ ] **Step 1: Add failing bounded-admission tests**

In `src/api/workflow_job.rs`, add tests that call the desired `inspect_completed_job` API:

```rust
#[test]
fn admission_counts_steps_and_validates_identifiers() {
    let body = serde_json::to_vec(&json!({
        "workflow_job": {
            "id": 41,
            "run_id": 31,
            "run_attempt": 2,
            "steps": [{"secret": "first"}, {"secret": "second"}]
        }
    }))
    .expect("fixture serializes");

    let admission = inspect_completed_job(&body).expect("admission is structurally valid");

    assert_eq!(admission.run_id().get(), 31);
    assert_eq!(admission.run_attempt().get(), 2);
    assert_eq!(admission.job_id().get(), 41);
    assert_eq!(admission.step_count(), 2);
    assert!(!format!("{admission:?}").contains("first"));
}
```

Add separate tests proving:

```rust
// Missing steps defaults to zero.
assert_eq!(inspect_completed_job(&missing_steps).unwrap().step_count(), 0);

// A non-array and invalid required IDs reject.
assert!(inspect_completed_job(&non_array_steps).is_none());
assert!(inspect_completed_job(&zero_job_id).is_none());
```

For the large-array test, serialize 2,048 ignored objects containing a forbidden string, inspect the
payload, assert `step_count() == 2_048`, and assert the admission debug output does not contain that
string. The production `StepCount` type must contain only `usize`, making retention of elements
unrepresentable.

- [ ] **Step 2: Run admission tests and verify RED**

Run:

```bash
cargo test api::workflow_job::tests::admission --lib
```

Expected: compilation fails because `inspect_completed_job` and `WorkflowJobAdmission` do not exist.

- [ ] **Step 3: Implement the allocation-bounded sequence visitor**

Add the Serde imports:

```rust
use std::fmt;
use serde::{
    de::{Error as _, IgnoredAny, SeqAccess, Visitor},
    Deserialize, Deserializer,
};
```

Create a private count-only type:

```rust
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StepCount(usize);

impl<'de> Deserialize<'de> for StepCount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct StepCountVisitor;

        impl<'de> Visitor<'de> for StepCountVisitor {
            type Value = StepCount;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an array of workflow steps")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut count = 0usize;
                while sequence.next_element::<IgnoredAny>()?.is_some() {
                    count = count
                        .checked_add(1)
                        .ok_or_else(|| A::Error::custom("workflow step count overflow"))?;
                }
                Ok(StepCount(count))
            }
        }

        deserializer.deserialize_seq(StepCountVisitor)
    }
}
```

Create derive-based envelope and projection structs containing only `id`, `run_id`, `run_attempt`,
and `#[serde(default)] steps: StepCount`. Construct `WorkflowJobAdmission` with validated workflow
newtypes. Its custom `Debug` implementation must print only the type name or redacted identifiers
plus the non-sensitive count; it must never expose raw identifiers accidentally through derived
formatting.

Implement:

```rust
pub(super) fn inspect_completed_job(body: &[u8]) -> Option<WorkflowJobAdmission> {
    let envelope: WorkflowJobAdmissionEnvelope = serde_json::from_slice(body).ok()?;
    Some(WorkflowJobAdmission {
        run_id: WorkflowRunId::new(envelope.workflow_job.run_id).ok()?,
        run_attempt: WorkflowRunAttempt::new(envelope.workflow_job.run_attempt).ok()?,
        job_id: WorkflowJobId::new(envelope.workflow_job.id).ok()?,
        step_count: envelope.workflow_job.steps.0,
    })
}
```

Add focused doc comments for the crate-visible type, function, and accessors, including parameters,
return behavior, and malformed-input behavior.

- [ ] **Step 4: Run workflow projection tests and verify GREEN**

Run:

```bash
cargo test api::workflow_job::tests --lib
```

Expected: admission tests and all existing detailed-projection tests pass.

- [ ] **Step 5: Run Clippy for the module changes**

```bash
cargo clippy --lib -- -D warnings
```

Expected: no warnings. Resolve lint findings without weakening lints or adding module-wide allows.

- [ ] **Step 6: Commit Task 2**

```bash
git add src/api/workflow_job.rs
git commit -m "feat: inspect workflow step counts boundedly"
```

---

### Task 3: Add workflow step and rejection metrics

**Files:**
- Modify: `src/metrics.rs`
- Modify: `src/app.rs`

**Interfaces:**
- Produces: `pub(crate) enum WorkflowTraceRejectionReason { TooManySteps }`
- Produces: `Metrics::observe_workflow_job_steps(&self, step_count: usize)`
- Produces: `Metrics::record_workflow_trace_rejection(&self, reason: WorkflowTraceRejectionReason)`
- Produces metric: `github_workflow_job_steps`
- Produces metric: `github_workflow_job_trace_rejections_total{reason="too_many_steps"}`

- [ ] **Step 1: Add failing metric tests**

In `src/metrics.rs`, add a focused test:

```rust
#[test]
fn workflow_job_metrics_observe_sizes_and_bounded_rejections() {
    let metrics = Metrics::new();

    metrics.observe_workflow_job_steps(0);
    metrics.observe_workflow_job_steps(36);
    metrics.observe_workflow_job_steps(1_500);
    metrics.record_workflow_trace_rejection(WorkflowTraceRejectionReason::TooManySteps);

    let exposition = metrics.encode().expect("metrics encode");
    for sample in [
        "github_workflow_job_steps_bucket{le=\"0.0\"} 1",
        "github_workflow_job_steps_bucket{le=\"40.0\"} 2",
        "github_workflow_job_steps_bucket{le=\"1024.0\"} 2",
        "github_workflow_job_steps_bucket{le=\"+Inf\"} 3",
        "github_workflow_job_steps_count 3",
        "github_workflow_job_steps_sum 1536.0",
        "github_workflow_job_trace_rejections_total{reason=\"too_many_steps\"} 1",
    ] {
        assert!(exposition.contains(sample), "missing {sample:?} in:\n{exposition}");
    }
}
```

Extend the startup-instrument test and `src/app.rs` metrics endpoint test to require both new metric
names before observations.

- [ ] **Step 2: Run metric tests and verify RED**

Run:

```bash
cargo test metrics::tests::workflow_job_metrics_observe_sizes_and_bounded_rejections --lib -- --exact
```

Expected: compilation fails because the workflow metric API does not exist.

- [ ] **Step 3: Implement closed metrics**

In `src/metrics.rs`, define buckets exactly as approved:

```rust
const WORKFLOW_JOB_STEP_BUCKETS: [f64; 10] = [
    0.0, 5.0, 10.0, 20.0, 40.0, 64.0, 128.0, 256.0, 512.0, 1_024.0,
];
```

Add the closed enum and label encoding:

```rust
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub(crate) enum WorkflowTraceRejectionReason {
    TooManySteps,
}

impl WorkflowTraceRejectionReason {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::TooManySteps => "too_many_steps",
        }
    }
}
```

Implement `EncodeLabelValue`, a private `WorkflowTraceRejectionLabels`, an unlabeled `Histogram`,
and a `CounterFamily<WorkflowTraceRejectionLabels>` in `MetricsInner`. Construct and register them
as:

```rust
registry.register(
    "workflow_job_steps",
    "Reported step count for structurally valid newly claimed completed workflow jobs",
    workflow_job_steps.clone(),
);
registry.register(
    "workflow_job_trace_rejections",
    "Completed workflow-job traces rejected by bounded reason",
    workflow_trace_rejections.clone(),
);
```

Seed `TooManySteps` at zero. Implement the two update methods with no repository, delivery, or job
arguments so identifiers cannot become labels through this API.

- [ ] **Step 4: Run metric and endpoint tests and verify GREEN**

Run:

```bash
cargo test metrics::tests --lib
cargo test app::tests::metrics_endpoint_is_public_and_exposes_every_required_instrument --lib -- --exact
```

Expected: all metric tests pass and startup exposition includes both instruments.

- [ ] **Step 5: Run metric privacy and concurrency tests**

Run:

```bash
cargo test metrics::tests::metric_updates_never_expose_untrusted_values --lib -- --exact
cargo test metrics::tests::clones_share_one_registry_during_concurrent_updates --lib -- --exact
```

Expected: both pass with no identifier leakage or registry regression.

- [ ] **Step 6: Commit Task 3**

```bash
git add src/metrics.rs src/app.rs
git commit -m "feat: observe workflow job step limits"
```

---

### Task 4: Enforce all-or-nothing admission and emit actionable diagnostics

**Files:**
- Modify: `src/api/webhook.rs`
- Modify: `src/telemetry/otlp_test.rs`

**Interfaces:**
- Consumes: `inspect_completed_job(&[u8]) -> Option<WorkflowJobAdmission>`
- Consumes: `AppState::workflow_job_max_steps() -> usize`
- Consumes: `Metrics::{observe_workflow_job_steps, record_workflow_trace_rejection}`
- Produces: private `record_workflow_trace_rejection(...)` warning helper in `src/api/webhook.rs`

- [ ] **Step 1: Add a failing exact-limit integration test**

Extend `WebhookTraceFixture` with a constructor that accepts `workflow_job_max_steps` and passes it
to `AppState::new`. Preserve `new()` at the default and existing exporter-timeout construction.

Add an OTLP integration test using a limit of `2` and exactly two valid steps. Assert:

```rust
assert_eq!(response.status(), StatusCode::NO_CONTENT);
assert_eq!(captured.child_count(job, "github.workflow.step"), 2);
assert_metric_line(&exposition, "github_workflow_job_steps_count 1");
assert_metric_line(&exposition, "github_workflow_job_steps_sum 2.0");
assert_metric_line(
    &exposition,
    "github_workflow_job_trace_rejections_total{reason=\"too_many_steps\"} 0",
);
```

- [ ] **Step 2: Run the exact-limit test and verify RED**

Run the new exact test by its final name:

```bash
cargo test telemetry::otlp_test::workflow_job_at_configured_step_limit_exports_complete_trace --lib -- --exact
```

Expected: the test fails because the handler does not inspect or observe workflow step counts.

- [ ] **Step 3: Add a failing over-limit diagnostic integration test**

Using a limit of `2`, send a valid job containing three steps and forbidden fields such as
`"commands":["secret-command"]` and `"output":"secret-output"`. Use fixed identifiers:

```text
repository_name=owner/repository
workflow_run_id=8801
workflow_run_attempt=2
workflow_job_id=9901
delivery_id=550e8400-e29b-41d4-a716-446655440801
step_count=3
step_limit=2
```

Assert:

- response is `204 No Content`;
- the delivery is durably claimed;
- no `github.workflow.job` or `github.workflow.step` span exists for the delivery;
- histogram count is 1 and sum is 3;
- `github_workflow_job_trace_rejections_total{reason="too_many_steps"}` is 1;
- structured stderr contains the reason and every approved identifier/numeric value;
- captured OTLP logs contain the fixed warning and approved identifiers after force flush;
- stderr, OTLP logs, spans, and metrics do not contain `secret-command` or `secret-output`;
- Prometheus exposition does not contain repository, delivery, run, or job identifiers.

- [ ] **Step 4: Run the rejection test and verify RED**

```bash
cargo test telemetry::otlp_test::workflow_job_over_step_limit_emits_actionable_rejection_without_trace --lib -- --exact
```

Expected: failure because over-limit jobs are still projected and no rejection telemetry exists.

- [ ] **Step 5: Implement webhook admission and warning**

Replace the direct completed-job projection block in `src/api/webhook.rs` with this control flow:

```rust
if event_type == EventType::WorkflowJob && action == Action::Completed {
    if let Some(admission) = workflow_job::inspect_completed_job(request.body.as_ref()) {
        state
            .metrics()
            .observe_workflow_job_steps(admission.step_count());
        let step_limit = state.workflow_job_max_steps();
        if admission.step_count() > step_limit {
            record_workflow_trace_rejection(
                &state,
                &request.repository_name,
                &request.delivery_id,
                &admission,
                step_limit,
            );
        } else if let Some(workflow_trace) = workflow_job::project_completed_job(
            request.body.as_ref(),
            &request.repository_name,
            &request.delivery_id,
            received_at,
        ) {
            state.workflow_trace_emitter().emit(&workflow_trace);
        }
    }
}
```

The private rejection helper must increment
`WorkflowTraceRejectionReason::TooManySteps`, normalize the delivery UUID through
`DeliveryId::encode_lower`, and emit exactly one parentless warning:

```rust
state
    .metrics()
    .record_workflow_trace_rejection(WorkflowTraceRejectionReason::TooManySteps);
let mut delivery_buffer = uuid::Uuid::encode_buffer();
warn!(
    parent: None,
    reason = WorkflowTraceRejectionReason::TooManySteps.as_str(),
    repository_name = repository_name.as_str(),
    workflow_run_id = admission.run_id().get(),
    workflow_run_attempt = admission.run_attempt().get(),
    workflow_job_id = admission.job_id().get(),
    delivery_id = delivery_id.encode_lower(&mut delivery_buffer),
    step_count = admission.step_count(),
    step_limit,
    "completed workflow-job trace rejected"
);
```

Keep this warning out of the active request span by using `parent: None`. Do not construct names,
SHAs, URLs, or payload-derived text for the warning.

- [ ] **Step 6: Run accepted and rejected integration tests and verify GREEN**

```bash
cargo test telemetry::otlp_test::workflow_job_at_configured_step_limit_exports_complete_trace --lib -- --exact
cargo test telemetry::otlp_test::workflow_job_over_step_limit_emits_actionable_rejection_without_trace --lib -- --exact
```

Expected: both tests pass.

- [ ] **Step 7: Add and run duplicate-rejection coverage**

Add a test that sends the same signed over-limit delivery twice through a limit-2 fixture. Assert:

```text
accepted webhook requests = 2
generic workflow_job.completed events = 1
duplicate deliveries = 1
workflow step histogram count = 1
workflow trace rejections = 1
completed workflow-job trace rejected warning occurrences = 1
historical workflow spans = 0
```

Run:

```bash
cargo test telemetry::otlp_test::duplicate_over_limit_workflow_job_records_one_rejection --lib -- --exact
```

First verify RED by adding the test before any duplicate-specific correction. If the existing claim
boundary already makes it GREEN immediately, temporarily assert two histogram observations to
prove the test detects the boundary, observe the expected failure, then restore the correct
assertion of one before proceeding.

- [ ] **Step 8: Run the complete workflow and privacy suites**

```bash
cargo test api::workflow_job::tests --lib
cargo test telemetry::otlp_test::workflow --lib
cargo test telemetry::otlp_test::duplicate_workflow_delivery_emits_one_historical_trace --lib -- --exact
cargo test telemetry::otlp_test::workflow_identifiers_and_names_are_span_only_and_payload_data_is_absent --lib -- --exact
```

Expected: accepted-job identifiers remain span-only, while only the explicit over-limit warning
uses the approved diagnostic exception.

- [ ] **Step 9: Commit Task 4**

```bash
git add src/api/webhook.rs src/telemetry/otlp_test.rs
git commit -m "feat: reject oversized workflow traces"
```

---

### Task 5: Document and fully validate the PR amendment

**Files:**
- Modify: `docs/operations.md`
- Create: `changelog/<current-timestamp>-workflow-job-step-limit.md`
- Modify only if a test exposes a defect: files from Tasks 1–4

**Interfaces:**
- Documents: `GHE_WORKFLOW_JOB_MAX_STEPS`
- Documents: `github_workflow_job_steps`
- Documents: `github_workflow_job_trace_rejections_total{reason="too_many_steps"}`
- Documents: structured-warning identifier exception and GitHub lookup procedure

- [ ] **Step 1: Update operations documentation**

In `docs/operations.md`, add a workflow processing configuration table or paragraph documenting:

```text
GHE_WORKFLOW_JOB_MAX_STEPS: default 256; valid range 1-1024; no unlimited value.
```

Amend “Completed workflow traces” to state:

- structurally valid newly claimed completed jobs update `github_workflow_job_steps`;
- the limit is inclusive;
- accepted jobs emit every reported step;
- over-limit jobs emit no partial trace;
- rejections increment
  `github_workflow_job_trace_rejections_total{reason="too_many_steps"}`;
- rejections retain `204 No Content` and the durable claim;
- the warning contains only the approved repository, delivery, run, attempt, job, count, and limit
  fields;
- operators can query `GET /repos/{owner}/{repo}/actions/jobs/{job_id}` using the repository and
  `workflow_job_id`, and use the delivery UUID to correlate GitHub webhook delivery records.

Update the privacy section so the identifier-bearing rejection warning is an explicit narrow
exception to the general spans-only rule.

- [ ] **Step 2: Add the required timestamped changelog**

Create a timestamped file under `changelog/` describing:

- configurable 256 default and 1,024 ceiling;
- bounded count-only admission pass;
- all-or-nothing rejection;
- histogram and rejection counter;
- actionable warning identifiers and retained forbidden-data boundary; and
- tests added for limit, metrics, privacy, and deduplication.

Use `date '+%Y-%m-%dT%H-%M-%S%z'` for the filename and do not use emoji.

- [ ] **Step 3: Run formatting**

```bash
just fmt
```

Expected: `cargo fmt --all -- --check` exits zero without modifying source files.

- [ ] **Step 4: Run build and Clippy**

```bash
cargo build
cargo clippy --all-targets -- -D warnings
```

Expected: both commands exit zero with no warnings.

- [ ] **Step 5: Run the complete test suite**

```bash
just test
```

Expected: all unit and integration tests pass, including workflow OTLP, logging, metrics, and
privacy tests.

- [ ] **Step 6: Build documentation**

```bash
cargo doc --no-deps
```

Expected: rustdoc exits zero with no warnings.

- [ ] **Step 7: Review the final diff and operational invariants**

```bash
git diff --check
git status --short
git diff --stat origin/main...HEAD
rg -n "GHE_WORKFLOW_JOB_MAX_STEPS|workflow_job_steps|too_many_steps" \
  src docs changelog tests
```

Confirm manually that:

- there is no unlimited configuration path;
- no `Vec<WorkflowStepProjection>` is constructed before the count comparison for oversized jobs;
- the rejection path cannot invoke `WorkflowTraceEmitter::emit`;
- no identifier is a Prometheus label;
- only the approved warning emits repository and workflow identifiers to logs; and
- no production `.unwrap()`, debug print, wildcard import, or lint suppression was added.

- [ ] **Step 8: Commit documentation and any validation fixes**

```bash
git add docs/operations.md changelog src tests
git commit -m "docs: operate bounded workflow traces"
```

If no validation fix changed `src/` or `tests/`, stage only `docs/operations.md` and the new
changelog file.

- [ ] **Step 9: Push and update PR #39**

```bash
git push origin feat-issue-10-workflow-job-traces
```

Then add a concise PR comment summarizing the 256 default, 1,024 hard maximum, exact step histogram,
all-or-nothing rejection, actionable warning fields, and successful validation commands.
