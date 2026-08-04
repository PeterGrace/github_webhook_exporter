# Health Checks and Graceful Lifecycle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add unauthenticated liveness/readiness endpoints and a signal-driven, time-bounded graceful HTTP shutdown that completes Specification 1 without leaking secrets.

**Architecture:** A focused `health` module owns health routes; liveness has no state dependency while readiness receives only a cheap `SqlitePool` clone and calls the existing storage probe. A focused `lifecycle` module normalizes SIGINT/SIGTERM and drives Axum graceful shutdown, first polling the server and shutdown future concurrently and then bounding the drain with Tokio time. `main` preserves the required startup sequence and logs only normalized lifecycle outcomes.

**Tech Stack:** Rust 2021, Axum 0.8, Tokio signals/synchronization/time, SQLx SQLite, tracing, Tower test utilities.

## Global Constraints

- `GET /health/live` is unauthenticated and never queries SQLite or external dependencies.
- `GET /health/ready` returns `200` only when the migrated SQLite pool passes `SELECT 1`; failures return `503` without details.
- SQLite open or migration failure remains fatal before bind.
- SIGTERM and SIGINT share one Tokio-native shutdown path; no synchronous signal crate is added.
- Draining stops admission and lasts at most `GHE_SHUTDOWN_TIMEOUT_SECONDS`.
- Logs, responses, and persisted data must not expose credentials, plaintext secrets, authorization headers, ciphertext, or nonces.
- Every production behavior is introduced by a failing test first.

---

### Task 1: Health endpoints

**Files:**
- Create: `src/health.rs`
- Modify: `src/lib.rs`
- Modify: `src/app.rs`
- Test: `src/health.rs`

**Interfaces:**
- Consumes: `storage::probe_database(&SqlitePool) -> Result<(), StorageError>`.
- Produces: `health::router(pool: SqlitePool) -> Router<AppState>` merged by `app::build_router`.

- [ ] **Step 1: Write failing router tests**

Add tests that call `/health/live` and `/health/ready` through `tower::ServiceExt::oneshot`. Close the pool before the liveness request to prove liveness still returns `200`; use a migrated temporary database for readiness `200`, then close its pool and assert readiness `503`. Assert readiness has an empty body so storage details cannot escape.

- [ ] **Step 2: Verify the tests fail**

Run: `cargo test health --lib`
Expected: FAIL because `crate::health` and both routes do not exist.

- [ ] **Step 3: Implement minimal handlers and state wiring**

Implement `async fn live() -> StatusCode` with no extractor and `async fn ready(State(pool): State<SqlitePool>) -> StatusCode` mapping a successful probe to `OK` and every failure to `SERVICE_UNAVAILABLE`. Store a cheap `SqlitePool` clone in `AppState`, expose only the clone needed to construct the health router, merge health and API routers, and document all public items and failure behavior.

- [ ] **Step 4: Verify health tests pass**

Run: `cargo test health --lib`
Expected: PASS for independent liveness, ready success, and redacted ready failure.

- [ ] **Step 5: Commit**

```bash
git add src/health.rs src/lib.rs src/app.rs
git commit -m "feat: add liveness and readiness endpoints"
```

### Task 2: Signal normalization and bounded graceful drain

**Files:**
- Create: `src/lifecycle.rs`
- Modify: `src/lib.rs`
- Modify: `src/app.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Test: `src/lifecycle.rs`
- Test: `src/app.rs`

**Interfaces:**
- Produces: `lifecycle::shutdown_signal() -> impl Future<Output = ShutdownSignal>` and public `ShutdownSignal::{Interrupt, Terminate}`.
- Produces: `app::serve_with_shutdown(listener, state, shutdown, timeout) -> Result<ShutdownOutcome, io::Error>` and `ShutdownOutcome::{Completed, TimedOut}`.

- [ ] **Step 1: Write failing lifecycle and drain tests**

Test a private generic signal selector with immediately-ready synthetic SIGINT and SIGTERM futures and assert the normalized variants. Add a real TCP drain test using a test router whose handler blocks on a `Notify`: start a request, trigger shutdown through a oneshot channel, release the handler, and assert its response completes and the server returns `Completed`. Add a `#[tokio::test(start_paused = true)]` test whose handler never completes, trigger shutdown, advance beyond the configured duration, and assert `TimedOut`.

- [ ] **Step 2: Verify the tests fail**

Run: `cargo test lifecycle --lib`
Expected: FAIL because lifecycle types and bounded serving do not exist.

- [ ] **Step 3: Implement the minimal Tokio-native lifecycle**

Enable Tokio `signal` and `sync` features and the dev-only `test-util` feature. Use `tokio::signal::ctrl_c` and Unix `tokio::signal::unix::SignalKind::terminate`; use a pending future as the non-Unix SIGTERM branch. In the serving helper, poll Axum and the shutdown future concurrently, notify Axum through a oneshot channel, and wrap only the post-signal drain in `tokio::time::timeout`. Return normalized outcomes without embedding request or configuration data.

- [ ] **Step 4: Verify lifecycle tests pass**

Run: `cargo test lifecycle --lib`
Expected: PASS for both signal sources, completed drains, and forced timeout.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/lifecycle.rs src/lib.rs src/app.rs
git commit -m "feat: bound graceful HTTP shutdown"
```

### Task 3: Startup wiring and operational regressions

**Files:**
- Modify: `src/main.rs`
- Modify: `tests/startup.rs`
- Modify: `tests/repository_api.rs`

**Interfaces:**
- Consumes: `RuntimeConfig::shutdown_timeout`, `lifecycle::shutdown_signal`, and `app::serve_with_shutdown`.
- Preserves: configuration, telemetry, storage/migrations, cipher/state, bind, and serve startup order.

- [ ] **Step 1: Write failing startup and persistence tests**

Add a process test with a database path whose parent does not exist and valid security values; assert nonzero exit, a normalized storage failure, and absence of the database path and credentials. Add an API restart test that creates a repository through one router/store, drops it, reopens the same database with the same key, builds a second router, and verifies the metadata remains available without secret fields.

- [ ] **Step 2: Verify the tests expose missing behavior**

Run: `cargo test --test startup --test repository_api`
Expected: the restart test compiles only after test harness support is added; startup redaction remains a required passing regression.

- [ ] **Step 3: Wire lifecycle into the binary**

Capture the listener's actual local address for logging, call `serve_with_shutdown` with `shutdown_signal()` and the configured timeout, log the normalized received signal, log successful completion, and warn on `TimedOut` without logging secret-bearing configuration. Keep database open/migration before bind so startup failure can never report readiness.

- [ ] **Step 4: Verify integration tests pass**

Run: `cargo test --test startup --test repository_api`
Expected: PASS for startup failure redaction, durable API metadata, and all existing API behavior.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs tests/startup.rs tests/repository_api.rs
git commit -m "feat: wire signal-driven service lifecycle"
```

### Task 4: Operations documentation and final security scan

**Files:**
- Create: `docs/operations.md`
- Create: `changelog/2026-08-04T12-52-09-0400-health-graceful-lifecycle.md`
- Modify: `tests/startup.rs`

**Interfaces:**
- Documents the public HTTP and process contracts delivered by Tasks 1-3.

- [ ] **Step 1: Add the failing end-to-end redaction assertion**

Extend process-level tests to scan captured stderr and health response bodies for unique admin-token, master-key, webhook-secret, authorization, ciphertext, and nonce sentinels while exercising startup failure and readiness failure. The production change that would make this fail is interpolating internal errors or configuration values into lifecycle logs or health responses.

- [ ] **Step 2: Run the security regression**

Run: `cargo test --test startup redaction -- --nocapture`
Expected: PASS only when every captured channel omits all sentinels and still contains normalized diagnostic context.

- [ ] **Step 3: Write operational and changelog documentation**

Document endpoint semantics, unauthenticated access, startup-fatal storage failures, SIGINT/SIGTERM behavior, timeout forcing, and concrete probe examples in `docs/operations.md`. Record code, tests, and operational behavior in the timestamped changelog without sensitive sample values.

- [ ] **Step 4: Commit**

```bash
git add docs/operations.md changelog/2026-08-04T12-52-09-0400-health-graceful-lifecycle.md tests/startup.rs
git commit -m "docs: describe health and shutdown operations"
```

### Task 5: Full validation and PR delivery

**Files:**
- Modify only files required by validation fixes.

- [ ] **Step 1: Run the mandatory project sequence**

Run in order:

```bash
just fmt
cargo clippy --all-targets -- -D warnings
just test
```

Expected: all commands exit `0`. If any fix is required, restart the sequence from `just fmt`.

- [ ] **Step 2: Run the remaining specification gates**

```bash
cargo build
cargo doc --no-deps
```

Expected: both commands exit `0` without warnings.

- [ ] **Step 3: Commit validation fixes if necessary**

```bash
git status --short
git add src tests docs changelog Cargo.toml Cargo.lock
git commit -m "fix: address lifecycle validation findings"
```

- [ ] **Step 4: Push and open the linked PR**

Push `feat-issue-5-health-graceful-lifecycle`, open a PR to `main` with actual validation evidence and `Closes #5`, then comment the PR URL on issue #5.
