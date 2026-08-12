# Workflow Span Context Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enrich every historical workflow job and step span with authoritative workflow trigger, branch context, and direct GitHub Actions log links when available.

**Architecture:** Project bounded context from authenticated `workflow_run` deliveries, persist it by repository/run/attempt, and look it up when a completed `workflow_job` is emitted. Store only normalized event and sanitized branch values, prune records with the delivery-retention cutoff, and omit unavailable context rather than inferring it.

**Tech Stack:** Rust, serde/serde_json, sqlx/SQLite, Axum, OpenTelemetry, Tokio.

## Global Constraints

- Use `workflow_run.event` as the authoritative trigger.
- Correlate by repository ID, positive workflow run ID, and positive run attempt.
- Export identical context on `github.workflow.job` and each `github.workflow.step` span.
- Normalize unsupported workflow events to `other`.
- Sanitize and bound branches before persistence; omit missing or ambiguous values.
- Derive GitHub Actions URLs only from the validated repository, run ID, job ID, and step number; ignore payload-provided URLs.
- Never add context or URLs to Prometheus labels or logs, and never persist full webhook payloads.
- Follow red-green-refactor and add a timestamped changelog entry.

---

### Task 1: Bounded workflow-run context model and projection

**Files:**
- Create: `src/api/workflow_run.rs`
- Modify: `src/api/mod.rs`
- Modify: `src/telemetry/workflow.rs`

**Interfaces:**
- Produces: `WorkflowEvent::normalize(Option<&str>)`, `WorkflowBranch::sanitize(&str)`, `WorkflowRunContext`, and `workflow_run::project_context(&[u8]) -> Option<WorkflowRunContext>`.

- [ ] Add failing unit tests for `pull_request`, `merge_group`, unknown events, control-character/length branch sanitization, positive IDs, `head_branch` source selection, unique PR branch fallback, and ambiguous target omission.
- [ ] Run `cargo test api::workflow_run::tests telemetry::workflow::tests` and verify failures are caused by missing types/module.
- [ ] Implement the minimal bounded types and serde projection without retaining raw payload fields.
- [ ] Re-run the focused tests and refactor only while green.

### Task 2: Durable correlation storage and retention

**Files:**
- Create: `migrations/202608120001_create_workflow_run_contexts.sql`
- Create: `src/storage/workflow_run_store.rs` with focused unit tests
- Modify: `src/storage/mod.rs`
- Modify: `src/telemetry/trace.rs`
- Modify: `src/retention.rs`
- Modify: `src/app.rs`

**Interfaces:**
- Produces: `WorkflowRunStore::{upsert,get,prune_batch}` keyed by `RepositoryId`, `WorkflowRunId`, and `WorkflowRunAttempt`.
- Consumes: validated `WorkflowRunContext`; returns only redacted `WorkflowRunStoreError` values.

- [ ] Add failing migration/storage tests for upsert/get, restart persistence, attempt separation, bounded overwrite, unknown repository failure, and 1,000-row pruning.
- [ ] Run `cargo test --test workflow_run_storage` and verify RED.
- [ ] Add the constrained table, indexes, traced SQL operations, and redacted store implementation.
- [ ] Re-run storage tests until GREEN.
- [ ] Add failing retention and app-state tests proving workflow context uses the delivery cutoff and is available to handlers.
- [ ] Wire the third prunable workload through application startup/shutdown and retention; rerun focused tests until GREEN.

### Task 3: Webhook correlation and OTLP enrichment

**Files:**
- Modify: `src/api/webhook.rs`
- Modify: `src/api/workflow_job.rs`
- Modify: `src/telemetry/workflow.rs`
- Modify: `src/telemetry/trace.rs`
- Modify: `src/telemetry/otlp_test.rs`
- Modify: `tests/webhook_api.rs`

**Interfaces:**
- `project_completed_job` consumes optional correlated `WorkflowRunContext`.
- Job and step attributes include `github.workflow.event`, `github.workflow.source_branch`, and `github.workflow.target_branch` only when represented by the bounded context.
- Job spans include `github.workflow.job.url`; step spans include `github.workflow.step.url` with `#step:<number>:1`, both derived exclusively from validated model values.

- [ ] Add failing webhook tests proving newly claimed workflow-run deliveries upsert context and duplicates do not mutate it.
- [ ] Add failing OTLP tests for identical job/step context on pull-request and merge-group traces, rerun attempt isolation, omission without context, derived job/step URLs, and privacy boundaries.
- [ ] Run focused webhook and OTLP tests and verify RED due to absent dispatch/enrichment.
- [ ] Dispatch workflow-run persistence after a new delivery claim; look up context before completed-job projection; treat store failures as the existing redacted unavailable path.
- [ ] Add centralized attribute builders and include context in both job and step attribute vectors.
- [ ] Re-run focused tests until GREEN and refactor duplicate attribute assembly.

### Task 4: Documentation and final verification

**Files:**
- Modify: `book/src/reference/traces.md`
- Modify: `book/src/explanation/design-decisions.md`
- Create: `changelog/2026-08-12T14-08-54Z-workflow-span-context.md`

- [ ] Document attribute names, correlation/retention semantics, omission behavior, rerun keys, derived URL formats, and privacy boundaries.
- [ ] Record the implementation and validation in the timestamped changelog.
- [ ] Run `just fmt`.
- [ ] Run `cargo clippy --all-targets -- -D warnings`.
- [ ] Run `just test`.
- [ ] Review the diff for raw payload retention, unbounded values, branch/event metric labels, logs, and unrelated changes.
