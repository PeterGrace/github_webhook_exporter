# Durable Merge-Queue Attempt State Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist one active merge-queue attempt per repository and pull request, then complete it exactly once with bounded typed outcomes across restarts and concurrent delivery.

**Architecture:** Add a schema-exact SQLite migration, validated queue-domain values, and a focused `MergeQueueStore`. Enqueue and completion operations use atomic SQL inside transactions; the public API accepts only validated identifiers/timestamps and evidence-backed completion constructors, while expected replays return typed transition results rather than errors.

**Tech Stack:** Rust 2021, Tokio, SQLx SQLite, `time`, `thiserror`, embedded SQLx migrations.

## Global Constraints

- Persist no complete webhook payload, repository name, delivery UUID, signature, SHA, raw dequeue reason, or secret.
- Permit at most one pending attempt for each repository and pull-request pair.
- Phase 3 can complete attempts only as `succeeded`/`pull_request_merged` or `unknown`/`unclassified_dequeue`.
- Keep `failed` and `cancelled` schema/type-supported but do not expose a Phase 3 completion constructor for them.
- Errors and debug output must reveal neither SQLite details nor attacker-controlled values.
- Repository deletion must cascade to queue attempts.
- Periodic scheduling, webhook projection/dispatch, metrics, and dequeue classification are out of scope.

---

### Task 1: Queue Schema and Validated Domain Values

**Files:**
- Create: `migrations/202608050001_create_merge_queue_attempts.sql`
- Create: `src/domain/merge_queue.rs`
- Modify: `src/domain/mod.rs`
- Modify: `Cargo.toml`
- Create: `tests/merge_queue_storage.rs`

**Interfaces:**
- Produces: `PullRequestNumber::new(i64) -> Result<Self, PullRequestNumberError>` and `get() -> i64`.
- Produces: `QueueTimestamp::parse(&str) -> Result<Self, QueueTimestampError>`, `from_datetime(OffsetDateTime) -> Result<Self, QueueTimestampError>`, and `as_str() -> &str`.
- Produces: bounded `QueueOutcome`, `QueueReasonCode`, and `QueueCompletion` values with only `pull_request_merged` and `unclassified_dequeue` public completion constructors.

- [ ] **Step 1: Write failing domain and migration tests**

Add unit tests rejecting non-positive pull-request numbers and malformed timestamps, accepting normalized RFC 3339 UTC timestamps, and asserting the fixed enum strings. Add an integration test that inspects every queue-attempt column, foreign key, check constraint, partial unique index, and completed-at index, and proves forbidden sensitive columns are absent.

- [ ] **Step 2: Run tests to verify RED**

Run: `cargo test --test merge_queue_storage`
Expected: FAIL because the migration and merge-queue domain module do not exist.

Run: `cargo test domain::merge_queue`
Expected: FAIL because `domain::merge_queue` does not exist.

- [ ] **Step 3: Implement the migration and minimal domain types**

Create the table and indexes exactly from Specification 3. Enable `time`'s `parsing` feature, parse RFC 3339 timestamps into `OffsetDateTime`, normalize to UTC millisecond text, keep fields private, and expose only bounded constructors/accessors.

- [ ] **Step 4: Run focused tests to verify GREEN**

Run: `cargo test domain::merge_queue && cargo test --test merge_queue_storage migration`
Expected: PASS.

### Task 2: Atomic Enqueue and Completion Transitions

**Files:**
- Create: `src/storage/merge_queue_store.rs`
- Modify: `src/storage/mod.rs`
- Modify: `tests/merge_queue_storage.rs`

**Interfaces:**
- Consumes: `RepositoryId`, `PullRequestNumber`, `QueueTimestamp`, and `QueueCompletion`.
- Produces: `MergeQueueStore::new(SqlitePool)`.
- Produces: `enqueue(RepositoryId, PullRequestNumber, &QueueTimestamp) -> Result<EnqueueTransition, MergeQueueStoreError>`.
- Produces: `complete(RepositoryId, PullRequestNumber, &QueueCompletion) -> Result<CompletionTransition, MergeQueueStoreError>`.
- Produces: `EnqueueTransition::{Created, AlreadyActive}` and `CompletionTransition::{Completed, AlreadyCompleted, MissingActiveAttempt}`.

- [ ] **Step 1: Write failing sequential transition tests**

Test first enqueue, repeated enqueue, merged completion, dequeue completion, repeated completion, missing completion, and enqueue after a terminal attempt. Query persisted rows directly to assert exact timestamps, outcomes, reasons, and row counts.

- [ ] **Step 2: Run tests to verify RED**

Run: `cargo test --test merge_queue_storage transition`
Expected: FAIL because `MergeQueueStore` and transition types do not exist.

- [ ] **Step 3: Implement minimal transactional operations**

Use an `INSERT ... ON CONFLICT ... WHERE completed_at IS NULL DO NOTHING` for enqueue. For completion, update only `completed_at IS NULL` inside one transaction; when no row changes, query for a terminal attempt in that same transaction to distinguish replay from missing state. Map busy/locked errors to `Unavailable` and discard all other SQL details behind `Internal`.

- [ ] **Step 4: Run focused tests to verify GREEN**

Run: `cargo test --test merge_queue_storage transition`
Expected: PASS.

### Task 3: Durability, Concurrency, Rollback, Cascade, and Pruning

**Files:**
- Modify: `src/storage/merge_queue_store.rs`
- Modify: `tests/merge_queue_storage.rs`

**Interfaces:**
- Produces: cloneable `MergeQueueStore` for concurrent Tokio tasks.
- Produces: `prune_completed_batch(OffsetDateTime) -> Result<u64, MergeQueueStoreError>`, deleting at most 1,000 terminal attempts per call while retaining pending and fresh terminal rows.

- [ ] **Step 1: Write failing lifecycle and failure-path tests**

Add tests for 16 concurrent enqueues, concurrent competing completions, database reopen between enqueue/completion, trigger-forced update rollback, locked database mapping, dropped-table internal error redaction, repository-delete cascade, and 1,000-row bounded completed-at pruning.

- [ ] **Step 2: Run tests to verify RED**

Run: `cargo test --test merge_queue_storage`
Expected: FAIL on missing pruning and any transition implementation that does not serialize correctly.

- [ ] **Step 3: Implement bounded pruning and correct any concurrency gaps**

Delete terminal rows selected by `completed_at < cutoff`, ordered by completion time and ID, limited to 1,000. Keep pending attempts ineligible. Preserve one transaction per state transition and rely on the partial unique index plus conditional update for serialization.

- [ ] **Step 4: Run focused tests to verify GREEN**

Run: `cargo test --test merge_queue_storage`
Expected: PASS with all lifecycle, concurrency, rollback, redaction, and retention assertions.

### Task 4: Documentation and Full Validation

**Files:**
- Modify: `src/lib.rs`
- Create: `changelog/2026-08-05T08-46-34-0400-durable-merge-queue-attempt-state.md`

**Interfaces:**
- Documents all new public modules, types, constructors, accessors, transition semantics, return values, and error behavior.

- [ ] **Step 1: Add the timestamped changelog entry and audit public documentation**

Document the migration, bounded domain model, transactional store behavior, tests, and explicit Phase 3 limits. Ensure public Rust items explain parameters, returns, and errors.

- [ ] **Step 2: Run the mandatory validation sequence**

Run in order:

```bash
just fmt
cargo build
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps
```

Expected: every command exits successfully with no warnings.

- [ ] **Step 3: Review scope and persisted-data contract**

Run `git diff --check`, inspect `git diff --stat`, and query the migration/test assertions to confirm no forbidden payload or identifier fields were added and no out-of-scope runtime dispatch, metrics, or scheduler changes were introduced.

- [ ] **Step 4: Commit the issue implementation**

```bash
git add Cargo.toml migrations src tests docs/superpowers/plans changelog
git commit -m "feat: add durable merge-queue attempt state

Closes #22"
```
