# Specialized Merge-Group Webhooks Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Record bounded specialized metrics exactly once for supported authenticated merge-group webhook deliveries without mutating pull-request attempt state.

**Architecture:** Add a focused `src/api/merge_group.rs` projection and dispatcher that parses the authenticated action and top-level reason once, then consumes the normalized event/action pair only after a durable new-delivery claim. The dispatcher discards non-string or unsupported values through the existing exact classifier and calls the closed `Metrics` API; the generic webhook path remains responsible for authentication, deduplication, and generic metrics.

**Tech Stack:** Rust, Axum, Serde/serde_json, prometheus-client, SQLx/SQLite, Tokio integration tests.

## Global Constraints

- Dispatch only after successful authentication and `DeliveryClaim::New`.
- `checks_requested` records reason `none`; `destroyed` maps exact lowercase `merged`, `dequeued`, and `invalidated`, with every other shape mapping to `other`.
- Unsupported actions retain generic metrics but produce no specialized metric.
- Duplicate deliveries produce neither generic nor specialized event increments.
- Merge-group events never call `MergeQueueStore` or create merge-queue attempt rows.
- Repository names, group IDs, SHAs, delivery UUIDs, raw reasons, signatures, secrets, and payload fragments must not enter metric labels or logs.
- Authenticated new and duplicate deliveries preserve the `204 No Content` response contract.

---

### Task 1: Specialized merge-group dispatch

**Files:**
- Create: `src/api/merge_group.rs`
- Modify: `src/api/mod.rs`
- Modify: `src/api/webhook.rs`
- Test: `tests/webhook_api.rs`

**Interfaces:**
- Consumes: `EventType`, `Action`, `MergeGroupAction`, `MergeGroupReason`, `normalize_merge_group_destroyed_reason`, and `Metrics::record_merge_group_event` from `src/metrics.rs`.
- Produces: `EventProjection::action(&self) -> Option<&str>` and `EventProjection::process_merge_group(&self, EventType, Action, &Metrics)`.

- [x] **Step 1: Write failing integration tests for supported actions**

Add signed merge-group requests for `checks_requested` and all four bounded destroyed outcomes. Assert `204`, one matching generic sample, one matching specialized sample, and zero rows in `merge_queue_attempts`.

- [x] **Step 2: Run the focused tests and verify RED**

Run: `cargo test --test webhook_api merge_group -- --nocapture`

Expected: FAIL because `github_merge_group_events_total` remains zero for every supported delivery.

- [x] **Step 3: Add the focused processor and new-delivery dispatch**

Implement a minimal top-level reason projection that accepts strings and treats missing/non-string values as absent. Normalize the event and action once in `webhook_handler`, record the generic event, then invoke `merge_group::process` only in the `DeliveryClaim::New` branch.

- [x] **Step 4: Run the focused tests and verify GREEN**

Run: `cargo test --test webhook_api merge_group -- --nocapture`

Expected: PASS.

### Task 2: Deduplication, unsupported input, and disclosure boundaries

**Files:**
- Modify: `tests/webhook_api.rs`
- Modify: `src/api/merge_group.rs` only if a failing test requires a correction.

**Interfaces:**
- Consumes: the Task 1 processor through the real Axum router.
- Produces: regression coverage for exact case-sensitive mapping, duplicate suppression, unsupported-action behavior, state isolation, and output redaction.

- [x] **Step 1: Write edge-case integration tests**

Use literal payloads for missing, null, numeric, mixed-case, unknown, and malicious destroyed reasons; require only the `other` specialized series to increment. Send an ordinary duplicate and require specialized and generic samples to remain at one while request/duplicate samples reach two. Send an unsupported merge-group action and require only its generic sample. Scan response, metrics, and captured logs for payload-derived identifiers and raw values. Assert the merge-queue attempt table remains empty.

- [x] **Step 2: Run the focused tests and identify whether behavior is incomplete**

Run: `cargo test --test webhook_api merge_group -- --nocapture`

Expected: any incomplete parser or dispatch branch fails with a mismatched literal metric count or disclosure assertion.

- [x] **Step 3: Make the minimal corrections (none required)**

Keep all input handling inside the focused projection. Do not add raw values to errors or tracing fields, and do not introduce merge-queue storage dependencies.

- [x] **Step 4: Run the focused tests and verify GREEN**

Run: `cargo test --test webhook_api merge_group -- --nocapture`

Expected: PASS.

### Task 3: Operations documentation and project validation

**Files:**
- Modify: `docs/operations.md`
- Create: `changelog/2026-08-05T10-13-06-0400-specialized-merge-group-webhooks.md`

**Interfaces:**
- Consumes: the completed HTTP behavior and metric names.
- Produces: operator guidance that group-level merged statistics are authoritative and remain separate from per-PR attempts.

- [x] **Step 1: Document behavior and security boundaries**

Explain supported actions, exact bounded reasons, duplicate semantics, unsupported-action behavior, and the prohibition on using group events to mutate pull-request attempt rows.

- [x] **Step 2: Add the timestamped changelog entry**

Summarize specialized dispatch, bounded normalization, test coverage, and state isolation without claiming unrun validation.

- [x] **Step 3: Run all mandatory gates from the repository root**

Run in order: `just fmt`, `cargo build`, `cargo clippy --all-targets -- -D warnings`, `just test`, and `cargo doc --no-deps`. If any command fails, correct the issue and rerun the full sequence.

- [x] **Step 4: Commit the complete scoped change**

Run: `git add src/api tests/webhook_api.rs docs/operations.md docs/superpowers/plans/2026-08-05-specialized-merge-group-webhooks.md changelog/2026-08-05T10-13-06-0400-specialized-merge-group-webhooks.md && git commit -m "feat: process specialized merge-group webhooks" -m "Closes #24"`
