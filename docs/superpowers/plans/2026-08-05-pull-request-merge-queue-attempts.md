# Pull-Request Merge-Queue Attempts Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Correlate authenticated `pull_request` queue events with durable attempts and bounded metrics across restarts.

**Architecture:** Successful HMAC authentication returns the repository's typed durable identifier. A focused pull-request queue processor receives that identifier, the normalized event/action, a minimal payload projection, one request receipt timestamp, `MergeQueueStore`, and `Metrics`; it maps committed store transitions to metric updates while leaving generic delivery claiming unchanged.

**Tech Stack:** Rust 2021, Axum, Tokio, SQLx/SQLite, serde/serde_json, time, prometheus-client, tracing.

## Global Constraints

- Specialized processing runs only after authentication and `DeliveryClaim::New`.
- Every dequeue is persisted and reported only as `unknown/unclassified_dequeue`; raw reasons are discarded.
- Queue-state persistence failure remains an authenticated `204`, increments `queue_state`, and emits one redacted correlated error.
- Outcome and duration metrics change only after a committed pending-to-completed transition.
- Replays, duplicate delivery UUIDs, and repeated enqueue operations do not update outcome or duration metrics.
- Missing completions increment only `missing_active_attempt`.
- Invalid durations increment only `invalid_duration`.
- SQLite and in-memory metrics are not exactly-once across process crashes.

---

### Task 1: Return Authenticated Repository Identity

**Files:**
- Modify: `src/storage/repository_store.rs`
- Modify: `src/security/webhook_auth.rs`
- Test: `src/security/webhook_auth.rs`
- Test: `tests/storage.rs`

**Interfaces:**
- Produces: `WebhookAuthenticator::authenticate(...) -> Result<RepositoryId, WebhookAuthenticationError>`.
- Produces: an internal repository authentication-material value containing `RepositoryId` and `RepositorySecret`.

- [ ] **Step 1: Write failing tests**

Assert that successful authentication returns the repository ID created by the fixture and that authentication material remains redacted and non-serializable.

- [ ] **Step 2: Verify RED**

Run: `cargo test authenticator_matches_the_official_github_sha256_fixture`
Expected: compilation failure because authentication still returns `()`.

- [ ] **Step 3: Implement the minimal identity flow**

Select `id` with encrypted secret columns, validate it as `RepositoryId`, decrypt into internal authentication material, verify HMAC, and return only the ID.

- [ ] **Step 4: Verify GREEN**

Run: `cargo test webhook_auth`
Expected: all webhook-authentication tests pass.

### Task 2: Return Completion Timing from the Transaction

**Files:**
- Modify: `src/storage/merge_queue_store.rs`
- Test: `tests/merge_queue_storage.rs`

**Interfaces:**
- Produces: `CompletionTransition::Completed { enqueued_at: QueueTimestamp }`.
- Preserves: `AlreadyCompleted` and `MissingActiveAttempt` no-op variants.

- [ ] **Step 1: Write a failing storage test**

Match the completed transition and assert that its returned enqueue timestamp equals the timestamp committed for the pending attempt.

- [ ] **Step 2: Verify RED**

Run: `cargo test --test merge_queue_storage sequential_transitions_are_exact_and_idempotent`
Expected: compilation failure because `Completed` has no enqueue timestamp.

- [ ] **Step 3: Implement transactional timing return**

Use `UPDATE ... RETURNING enqueued_at`, validate the returned canonical timestamp, and preserve the existing completed/missing lookup in the same transaction.

- [ ] **Step 4: Verify GREEN**

Run: `cargo test --test merge_queue_storage`
Expected: all merge-queue storage tests pass, including concurrency and rollback.

### Task 3: Process Pull-Request Queue Events

**Files:**
- Create: `src/api/pull_request.rs`
- Modify: `src/api/mod.rs`
- Modify: `src/api/webhook.rs`
- Modify: `src/app.rs`
- Test: `src/api/pull_request.rs`
- Test: `tests/webhook_api.rs`

**Interfaces:**
- Consumes: authenticated `RepositoryId`, `EventType`, `Action`, receipt `OffsetDateTime`, `MergeQueueStore`, and `Metrics`.
- Produces: durable enqueue/dequeue/merged-close transitions and bounded completion/failure metrics.

- [ ] **Step 1: Write failing signed-router tests**

Add tests for enqueue/dequeue, merged close, unmerged close, unsupported actions, malformed/absent timestamps, repeated transitions, duplicate deliveries, missing completion, restart completion, concurrent transitions, invalid durations, raw-reason redaction, and queue-state failure preserving `204`.

- [ ] **Step 2: Verify RED**

Run: `cargo test --test webhook_api pull_request_queue`
Expected: assertions fail because pull-request events do not mutate attempts or queue metrics.

- [ ] **Step 3: Implement the focused processor**

Deserialize only `pull_request.number`, `pull_request.updated_at`, and `pull_request.merged`; normalize timestamps with receipt fallback; dispatch enqueue/dequeue/merged-close only for normalized pull-request events; map all store transitions exhaustively; calculate duration only from the committed enqueue timestamp and completion timestamp.

- [ ] **Step 4: Preserve the queue failure boundary**

On store error, increment `FailureStage::QueueState`, generate one opaque local correlation ID, emit one redacted error, discard the error, and return `204` because the delivery claim has already committed.

- [ ] **Step 5: Verify GREEN**

Run: `cargo test --test webhook_api`
Expected: all webhook API tests pass with exact state and metric assertions.

### Task 4: Documentation and Full Validation

**Files:**
- Modify: `docs/operations.md`
- Create: `changelog/2026-08-05T10-32-03-0400-pull-request-merge-queue-attempts.md`

- [ ] **Step 1: Document operational semantics**

Describe supported pull-request transitions, unknown dequeue classification, timestamp fallback, idempotency, redaction, the at-most-once queue-processing boundary, and the state/metrics crash boundary.

- [ ] **Step 2: Add the timestamped changelog**

Record the authenticated repository identity flow, durable processor behavior, metric semantics, failure behavior, tests, and documentation changes.

- [ ] **Step 3: Run all project gates in order**

Run:

```bash
just fmt
cargo build
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps
```

Expected: every command exits successfully with no warnings.

- [ ] **Step 4: Commit, push, open, and link the PR**

Commit with `feat: track pull-request merge-queue attempts` and `Closes #25`, push the current issue branch, open a PR against `main`, and comment its URL on issue #25.
