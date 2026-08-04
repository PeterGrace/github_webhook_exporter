# Durable Delivery Claims Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Persist authenticated webhook delivery UUIDs atomically and prune expired claims in bounded SQLite batches.

**Architecture:** A validated `DeliveryId` newtype owns `uuid::Uuid`, while a focused `DeliveryStore` owns a cloned `SqlitePool`. Claims use one `INSERT ... ON CONFLICT DO NOTHING`; pruning uses one indexed `DELETE` selecting at most 1,000 row IDs. Store errors expose only stable unavailable/internal categories, and scheduling remains outside storage.

**Tech Stack:** Rust 2021, Tokio, SQLx SQLite, `uuid`, `time`, `thiserror`

## Global Constraints

- Store only canonical delivery UUID text and receipt timestamps.
- Never store payloads, signatures, repository identities, or secrets.
- One claim statement must distinguish new and duplicate deliveries atomically.
- Each prune operation deletes at most 1,000 rows.
- Busy and locked SQLite failures map to unavailable; all other failures remain redacted.
- A crash after claim commit and before metric increment may undercount one delivery; exactly-once metrics are not promised.

---

### Task 1: Typed Delivery Identifier

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `src/domain/delivery.rs`
- Modify: `src/domain/mod.rs`

**Interfaces:**
- Produces: `DeliveryId::parse(&str) -> Result<DeliveryId, DeliveryIdError>`
- Produces: `DeliveryId::encode_lower(&mut Uuid::encode_buffer()) -> &str` through a crate-private storage helper

- [ ] **Step 1: Add failing domain tests**

Add table-driven tests proving canonical UUID input succeeds and malformed, truncated, and extended input fails. The expected normalized value is the literal `550e8400-e29b-41d4-a716-446655440000`.

- [ ] **Step 2: Verify the tests fail**

Run: `cargo test domain::delivery --lib`
Expected: FAIL because `domain::delivery` and `DeliveryId` do not exist.

- [ ] **Step 3: Add the minimal typed implementation**

Add `uuid = { version = "1", default-features = false }`. Implement a private-field `DeliveryId(Uuid)` and redacted `DeliveryIdError`; parse with `Uuid::parse_str` and normalize accepted values to lowercase hyphenated text for storage. Add complete public documentation.

- [ ] **Step 4: Verify the domain tests pass**

Run: `cargo test domain::delivery --lib`
Expected: PASS.

### Task 2: Migration and Atomic Claim Store

**Files:**
- Create: `migrations/202608040002_create_processed_deliveries.sql`
- Create: `src/storage/delivery_store.rs`
- Modify: `src/storage/mod.rs`
- Create: `tests/delivery_storage.rs`

**Interfaces:**
- Consumes: `DeliveryId`
- Produces: `DeliveryStore::new(SqlitePool) -> DeliveryStore`
- Produces: `DeliveryStore::claim(&DeliveryId) -> Result<DeliveryClaim, DeliveryStoreError>`
- Produces: `DeliveryClaim::{New, Duplicate}`
- Produces: `DeliveryStoreError::{Unavailable, Internal}` with redacted `Display` and `Debug`

- [ ] **Step 1: Add failing migration and claim integration tests**

Test the exact schema columns, `delivery_id` primary key, and `processed_deliveries_received_at_idx`. Test first claim is `New`, a second claim is `Duplicate`, the first receipt timestamp is unchanged, the row survives pool close/reopen, and the table has no forbidden payload/signature/repository/secret columns.

- [ ] **Step 2: Verify the tests fail**

Run: `cargo test --test delivery_storage -- --nocapture`
Expected: FAIL because the migration and store API do not exist.

- [ ] **Step 3: Implement migration and one-statement claim**

Create the specified table and index. Implement:

```sql
INSERT INTO processed_deliveries (delivery_id, received_at)
VALUES (?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))
ON CONFLICT(delivery_id) DO NOTHING
```

Map `rows_affected() == 1` to `New` and `0` to `Duplicate`. Format the UUID into `Uuid::encode_buffer()` so claim persistence does not allocate a temporary `String`.

- [ ] **Step 4: Verify claim tests pass**

Run: `cargo test --test delivery_storage -- --nocapture`
Expected: PASS for schema, durability, duplicate, and forbidden-column tests.

### Task 3: Concurrency, Redacted Errors, and Bounded Retention

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `src/storage/delivery_store.rs`
- Modify: `tests/delivery_storage.rs`

**Interfaces:**
- Produces: `DeliveryStore::prune_batch(OffsetDateTime) -> Result<u64, DeliveryStoreError>`
- Uses: `const DELIVERY_PRUNE_BATCH_SIZE: i64 = 1_000` for SQLite's signed `LIMIT` bind

- [ ] **Step 1: Add failing behavior tests**

Add real-SQLite tests proving concurrent claims return exactly one `New`; a held `BEGIN IMMEDIATE` lock maps claim and prune to `Unavailable`; unexpected schema failure renders neither SQL nor SQLite details through `Display` or `Debug`; and controlled old/fresh timestamps prove each prune deletes at most 1,000 rows, preserves fresh rows, and repeated calls drain all expired rows.

- [ ] **Step 2: Verify the new tests fail**

Run: `cargo test --test delivery_storage -- --nocapture`
Expected: FAIL because pruning and delivery-specific error mapping are incomplete.

- [ ] **Step 3: Implement bounded indexed pruning and error mapping**

Add direct `time = { version = "0.3", default-features = false, features = ["formatting", "macros"] }`. Normalize the cutoff to UTC and format it in Rust using the same fixed-width millisecond text format used by claims, then execute one statement per call:

```sql
DELETE FROM processed_deliveries
WHERE delivery_id IN (
    SELECT delivery_id
    FROM processed_deliveries
    WHERE received_at < ?
    ORDER BY received_at, delivery_id
    LIMIT 1000
)
```

Return `rows_affected()`. Reuse `sqlite_is_busy_or_locked`; discard unexpected SQLx details and implement a manual redacted `Debug` for `DeliveryStoreError`.

- [ ] **Step 4: Verify all delivery storage tests pass**

Run: `cargo test --test delivery_storage -- --nocapture`
Expected: PASS.

### Task 4: Documentation and Full Validation

**Files:**
- Create: `changelog/2026-08-04T14-40-20-0400-durable-delivery-claims.md`

**Interfaces:**
- Documents: crash boundary, atomic duplicate behavior, 1,000-row operation limit, and validation evidence

- [ ] **Step 1: Write the changelog entry**

Document the new migration, UUID value object, atomic claim contract, bounded prune API, redacted errors, tests, and the explicit claim-commit/metric-update undercount boundary.

- [ ] **Step 2: Run all required validation gates**

Run in order:

```bash
just fmt
cargo build
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps
```

Expected: all commands pass without warnings.

- [ ] **Step 3: Review the diff for scope and secrets**

Run: `git diff --check && git status --short && git diff --stat`
Expected: no whitespace errors, only issue 13 files changed, and no payload, signature, repository identity, or secret persistence.
