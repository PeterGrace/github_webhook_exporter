# Phase 2 Lifecycle and Security Completion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete Phase 2 with accurate repository metrics, scheduled delivery retention, one bounded shutdown path, correlated safe internal errors, and integrated security regressions.

**Architecture:** Initialize the existing shared `Metrics` from a durable repository count before binding the listener, then update its gauge only after committed creates/deletes. Run Axum and a focused retention future under one Tokio `watch` cancellation signal and one timeout. Represent internal failure correlation with a typed UUID newtype carried into both structured logs and fixed-shape safe JSON responses.

**Tech Stack:** Rust 2021, Axum 0.8, Tokio, SQLx/SQLite, prometheus-client, tracing, UUID, time.

## Global Constraints

- Prune only claims older than `GHE_DELIVERY_RETENTION_DAYS`, in batches of at most 1,000, at `GHE_DELIVERY_PRUNE_INTERVAL_SECONDS`.
- Stop scheduling batches after SIGINT/SIGTERM; allow active work and HTTP requests at most `GHE_SHUTDOWN_TIMEOUT_SECONDS` total.
- Never expose payloads, repository identities, delivery UUIDs, signatures, authorization headers, plaintext secrets, ciphertext, nonces, or credentials in logs or error responses.
- Correlation IDs are opaque local UUIDs and never metric labels.
- Preserve the documented claim-before-counter undercount boundary; do not claim exactly-once metrics.
- Follow test-first red-green-refactor cycles and add a timestamped entry under `changelog/`.

---

### Task 1: Repository count gauge lifecycle

**Files:**
- Modify: `src/storage/repository_store.rs`
- Modify: `src/metrics.rs`
- Modify: `src/app.rs`
- Modify: `src/api/repositories.rs`
- Modify: `src/main.rs`
- Test: `tests/repository_api.rs`
- Test: `tests/startup.rs`

**Interfaces:**
- Produces: `RepositoryStore::count(&self) -> Result<u64, RepositoryStoreError>`.
- Produces: `AppState::initialize_repository_metrics(&self) -> Result<(), RepositoryStoreError>`.
- Produces: bounded `Metrics::increment_repository_configurations` and `decrement_repository_configurations` methods.

- [ ] **Step 1: Write failing persistence and router tests**

Add tests that seed durable repositories, initialize a fresh state, and assert `/metrics` exposes the durable count; assert successful POST/DELETE changes the gauge and conflict/not-found responses do not.

- [ ] **Step 2: Verify the tests fail for the missing initialization/update behavior**

Run: `cargo test --test repository_api repository_configuration_gauge -- --nocapture`
Expected: FAIL because the gauge remains zero.

- [ ] **Step 3: Implement the narrow count query and bounded gauge methods**

Use `SELECT COUNT(*) FROM repositories`, checked `i64` to `u64` conversion, startup `set`, and post-commit gauge increment/decrement calls only on successful handlers.

- [ ] **Step 4: Verify repository gauge tests pass**

Run: `cargo test --test repository_api repository_configuration_gauge -- --nocapture`
Expected: PASS.

### Task 2: Scheduled retention and shared graceful lifecycle

**Files:**
- Create: `src/retention.rs`
- Modify: `src/lib.rs`
- Modify: `src/app.rs`
- Modify: `src/main.rs`
- Test: `src/retention.rs`
- Test: `src/app.rs`
- Test: `tests/startup.rs`

**Interfaces:**
- Produces: `RetentionConfig::new(interval: Duration, retention: Duration) -> Result<RetentionConfig, RetentionError>`.
- Produces: `run_delivery_retention(store: DeliveryStore, config: RetentionConfig, shutdown: watch::Receiver<bool>) -> ()`.
- Produces: lifecycle serving that drives Axum and retention with one shutdown sender and one timeout.

- [ ] **Step 1: Write paused-time retention tests**

Use real migrated SQLite and `#[tokio::test(start_paused = true)]` to prove no immediate prune, interval-triggered repeated 1,000-row batches, fresh-row preservation, and cancellation between batches.

- [ ] **Step 2: Verify retention tests fail because the runner is absent**

Run: `cargo test retention::tests -- --nocapture`
Expected: FAIL to compile because the retention module/API does not exist.

- [ ] **Step 3: Implement the minimal retention runner**

Use `tokio::time::interval_at(Instant::now() + interval, interval)`, a fixed cutoff per pass, `tokio::select!` against `watch::Receiver::changed`, and continue while a batch returns exactly 1,000. Emit only normalized `outcome`, `batches`, and `deleted` fields.

- [ ] **Step 4: Verify retention tests pass**

Run: `cargo test retention::tests -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Write a failing shared-deadline drain test**

Extend app lifecycle tests with a controlled active background future and in-flight HTTP request; assert both complete before the deadline and timeout together when held.

- [ ] **Step 6: Verify the shared-deadline test fails under the HTTP-only coordinator**

Run: `cargo test app::tests::graceful_lifecycle -- --nocapture`
Expected: FAIL because background work is not coordinated.

- [ ] **Step 7: Evolve the coordinator and process startup**

Create one `watch` channel, make Axum and retention observe it, and wrap their joined drain in the existing shutdown timeout. Pass validated retention configuration from `RuntimeConfig`; normalize signal, completion, and timeout logs.

- [ ] **Step 8: Verify lifecycle and process signal tests pass**

Run: `cargo test app::tests::graceful_lifecycle -- --nocapture && cargo test --test startup -- --nocapture`
Expected: PASS for SIGINT, SIGTERM, completed drain, and timeout behavior.

### Task 3: Opaque correlated internal failures

**Files:**
- Modify: `src/error.rs`
- Modify: `src/api/webhook.rs`
- Modify: `tests/repository_api.rs`
- Modify: `tests/webhook_api.rs`

**Interfaces:**
- Produces: `ErrorCorrelationId` UUID newtype with redacted/opaque display.
- Produces: internal error constructors that preserve one ID through log and JSON `{code,message,error_id}`.

- [ ] **Step 1: Write failing response/log correlation tests**

Capture tracing for repository corruption and webhook database failure; parse the safe JSON `error_id`, assert it is a UUID appearing once in local logs, and scan outputs for all forbidden fixture values.

- [ ] **Step 2: Verify tests fail because responses omit correlation IDs**

Run: `cargo test --test repository_api redaction -- --nocapture && cargo test --test webhook_api database_failures -- --nocapture`
Expected: FAIL on missing `error_id`.

- [ ] **Step 3: Implement typed correlation propagation**

Generate the ID at the application error boundary, log only normalized stage/outcome plus the ID, and serialize it in safe internal/503 responses. Do not add it to `Metrics` APIs.

- [ ] **Step 4: Verify correlation and redaction tests pass**

Run: `cargo test --test repository_api redaction -- --nocapture && cargo test --test webhook_api database_failures -- --nocapture`
Expected: PASS with matching IDs and no forbidden values.

### Task 4: Integrated regression, operations documentation, and changelog

**Files:**
- Modify: `tests/webhook_api.rs`
- Modify: `tests/startup.rs`
- Modify: `docs/operations.md`
- Create: `changelog/2026-08-05T07-00-23-0400-phase-2-lifecycle-security.md`

**Interfaces:**
- Consumes: repository metric, retention, lifecycle, and correlation APIs from Tasks 1-3.
- Produces: reviewer-verifiable end-to-end coverage and operational contracts.

- [ ] **Step 1: Add the failing integrated restart/deduplication regression**

Configure a repository, rebuild state from the same SQLite file, submit the same correctly signed delivery twice, and assert one durable claim, one event increment, one duplicate increment, and no forbidden fixture values in response/exposition/database fields where prohibited.

- [ ] **Step 2: Verify the integrated test fails on missing startup initialization**

Run: `cargo test --test webhook_api restart -- --nocapture`
Expected: FAIL before all lifecycle behavior is wired.

- [ ] **Step 3: Complete integration wiring and documentation**

Document retention scheduling/batching, shared shutdown deadline, repository gauge semantics, correlation IDs, duplicate semantics, and the claim-before-counter crash undercount boundary. Add the required timestamped changelog.

- [ ] **Step 4: Run the full project gate from the repository root**

Run in order:

```bash
just fmt
cargo build
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps
```

Expected: every command exits zero with no warnings.

- [ ] **Step 5: Commit the scoped implementation**

```bash
git add src tests docs changelog
git commit -m "feat: complete Phase 2 lifecycle and security regressions

Closes #16"
```
