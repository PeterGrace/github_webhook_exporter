# Phase 3 Retention and Lifecycle Regressions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Retain completed merge-queue attempts for a configurable bounded period and run delivery and queue pruning under the service's existing cancellation and shutdown deadline.

**Architecture:** Extend validated runtime configuration with queue retention while retaining the one existing prune cadence. Generalize the retention runner to own one skipped-tick ticker and invoke two focused, bounded store operations with a fixed cutoff per workload; wire both stores through `AppState` into the same watch-based cancellation and Axum drain deadline.

**Tech Stack:** Rust 2021, Tokio watch channels and paused time, Axum, SQLx SQLite, `time`, `tracing`, existing integration-test helpers.

## Global Constraints

- `GHE_MERGE_QUEUE_RETENTION_DAYS` defaults to `90` and must be a positive `u64` whose conversion to seconds cannot overflow.
- Queue pruning reuses `GHE_DELIVERY_PRUNE_INTERVAL_SECONDS`; no second queue interval is introduced.
- Each SQLite operation deletes at most 1,000 rows and only completed attempts older than a pass-fixed cutoff.
- Pending attempts and fresh completed attempts remain untouched.
- Missed ticker events are skipped, and shared cancellation prevents any new batch from starting.
- Active SQLite operations and HTTP requests share `GHE_SHUTDOWN_TIMEOUT_SECONDS`.
- Logs expose only normalized workload/outcome/count fields and opaque correlation IDs, never row identities, SQL text, repository identities, PR numbers, timestamps, or payload data.
- Group-level `merged` remains authoritative; PR dequeue remains `unknown/unclassified_dequeue`; queue processing and metrics remain at-most-once after delivery claim.

---

### Task 1: Validated Merge-Queue Retention Configuration

**Files:**
- Modify: `src/config.rs`

**Interfaces:**
- Produces: `RuntimeConfig::merge_queue_retention(&self) -> Duration`.
- Extends: `RuntimeConfig::from_lookup` with `GHE_MERGE_QUEUE_RETENTION_DAYS` validation.

- [ ] **Step 1: Add failing configuration tests**

Extend the defaults assertion with `Duration::from_secs(90 * 86_400)`, the override fixture with `GHE_MERGE_QUEUE_RETENTION_DAYS=180`, and the redacted invalid-case table with zero, malformed, and `18446744073709551615`. Add a Unix-only non-Unicode value assertion for the new variable alongside existing optional-string validation.

- [ ] **Step 2: Verify RED**

Run: `cargo test config::tests`
Expected: compilation fails because `merge_queue_retention` does not exist.

- [ ] **Step 3: Implement the minimal configuration field and accessor**

Add `DEFAULT_MERGE_QUEUE_RETENTION_DAYS: u64 = 90`, parse with `optional_positive_u64`, use `checked_mul(SECONDS_PER_DAY)`, store a `Duration`, expose the documented accessor, and include only the duration in redacted `Debug` output.

- [ ] **Step 4: Verify GREEN**

Run: `cargo test config::tests`
Expected: all configuration tests pass.

### Task 2: Shared Bounded Retention Runner

**Files:**
- Modify: `src/retention.rs`

**Interfaces:**
- Replaces: `RetentionConfig::new(interval, delivery_retention)` with `RetentionConfig::new(interval, delivery_retention, merge_queue_retention)`.
- Produces: `run_retention(delivery_store: DeliveryStore, merge_queue_store: MergeQueueStore, config: RetentionConfig, shutdown: watch::Receiver<bool>)`.
- Keeps: `DeliveryStore::prune_batch` and `MergeQueueStore::prune_completed_batch` as one-operation boundaries.

- [ ] **Step 1: Add failing queue scheduling, failure, and cancellation tests**

Add tests proving one interval removes 1,005 expired queue attempts through two bounded operations while preserving one fresh completed and one old pending attempt; a dropped queue table emits `workload="merge_queue"`, `outcome="failed"`, and a UUID correlation ID without SQL details; cancellation before the tick preserves expired rows; and a failed queue pass can succeed at the next interval after the table is restored.

- [ ] **Step 2: Verify RED**

Run: `cargo test retention::tests`
Expected: compilation fails because the shared runner/configuration interfaces do not exist.

- [ ] **Step 3: Implement one ticker and two focused pruning loops**

Use `interval_at(Instant::now() + interval, interval)` and `MissedTickBehavior::Skip`. Calculate one delivery cutoff and one queue cutoff at the beginning of each scheduled pass. Run delivery pruning, check cancellation, then run queue pruning. Before every store call, check the same watch receiver; stop a workload on a short batch or failure. Emit normalized `workload`, `outcome`, `batches`, and `deleted` fields, adding only an opaque `ErrorCorrelationId` on failures.

- [ ] **Step 4: Verify GREEN**

Run: `cargo test retention::tests`
Expected: all retention tests pass under paused and real Tokio time.

### Task 3: Application Wiring and Shared Drain

**Files:**
- Modify: `src/app.rs`
- Modify: `src/main.rs`

**Interfaces:**
- `serve_with_shutdown` passes cloned delivery and merge-queue stores into `run_retention`.
- `main` constructs one `RetentionConfig` from delivery interval, delivery retention, and merge-queue retention.

- [ ] **Step 1: Add a failing shared-drain test**

Extend the existing background lifecycle test so a cancellation-aware background future models two active retention workloads and proves the server does not report `Completed` until both active work and an in-flight HTTP request are released. Keep the timeout test proving the same deadline aborts remaining work.

- [ ] **Step 2: Verify RED**

Run: `cargo test app::tests::graceful_lifecycle_waits_for_active_background_work`
Expected: the strengthened assertion fails against the one-workload test fixture or the new retention constructor fails to compile at call sites.

- [ ] **Step 3: Wire both stores and the third retention argument**

Clone `state.delivery_store` and `state.merge_queue_store` before router construction, invoke `run_retention` from the existing single background task, and add `config.merge_queue_retention()` to startup validation. Preserve one watch cancellation sender and one timeout-wrapped drain.

- [ ] **Step 4: Verify GREEN**

Run: `cargo test app::tests && cargo test --test startup`
Expected: lifecycle unit tests and SIGINT/SIGTERM process tests pass.

### Task 4: Integrated Queue Semantics and Security Regressions

**Files:**
- Modify: `tests/webhook_api.rs`

**Interfaces:**
- Uses the existing authenticated router, durable SQLite pool, webhook request helpers, and Prometheus exposition helper.

- [ ] **Step 1: Strengthen the restart test before changing production code**

Update `pull_request_queue_attempt_completes_after_database_restart` to send the enqueue delivery twice before restart and the merged completion twice after restart. Assert one durable terminal row, one outcome observation, two duplicates, and no merge-group metric/state coupling.

- [ ] **Step 2: Verify RED or mutation sensitivity**

Run: `cargo test --test webhook_api pull_request_queue_attempt_completes_after_database_restart`
Expected: the strengthened test passes existing semantics; then temporarily change one expected duplicate count to prove the assertion fails, restore it, and rerun. This task locks already implemented issue #25 behavior rather than adding production behavior.

- [ ] **Step 3: Add an integrated forbidden-data scan**

Capture stderr, response bodies, metrics exposition, and relevant SQLite text/bytes for malicious queue and group payloads. Assert absence of admin token, master key, plaintext webhook secret, authorization header, payload fragment, repository name, delivery/group IDs, PR number where forbidden, SHA, raw dequeue reason, signature, ciphertext, and nonce.

- [ ] **Step 4: Verify GREEN**

Run: `cargo test --test webhook_api`
Expected: all webhook, duplicate, restart, queue failure, group authority, and security regressions pass.

### Task 5: Operations Documentation, Changelog, and Full Validation

**Files:**
- Modify: `docs/operations.md`
- Modify: `src/lib.rs`
- Create: `changelog/<timestamp>-phase-3-retention-lifecycle.md`

**Interfaces:**
- Documents configuration, shared scheduling/shutdown, bounded queue retention, failure recovery, classification limits, and at-most-once boundaries.

- [ ] **Step 1: Update operational documentation and module description**

Document `GHE_MERGE_QUEUE_RETENTION_DAYS` default `90`, shared delivery prune interval, completed-only queue deletion, 1,000-row operation cap, retry-on-next-interval behavior, common shutdown deadline, authoritative group success, unknown PR dequeues, and non-exactly-once metric/state crash boundaries.

- [ ] **Step 2: Add the timestamped changelog entry**

Record the configuration, runner generalization, lifecycle wiring, tests, and documentation delivered by issue #26 under `changelog/` using the current timestamp.

- [ ] **Step 3: Run the mandatory validation sequence**

Run in order:

```bash
just fmt
cargo build
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps
```

Expected: every command exits successfully without warnings.

- [ ] **Step 4: Audit scope and sensitive-data boundaries**

Run `git diff --check`, inspect `git diff --stat`, and search changed production code for raw identifiers or payload logging. Confirm no second prune interval, replay store, dequeue classifier, or merge-group-to-PR join was introduced.

- [ ] **Step 5: Commit the implementation**

```bash
git add src tests docs changelog
git commit -m "feat: complete Phase 3 retention lifecycle

Closes #26"
```
