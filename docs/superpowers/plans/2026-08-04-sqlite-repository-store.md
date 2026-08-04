# SQLite Repository Store Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add hardened SQLite startup, embedded migrations, and a transactional encrypted repository configuration store.

**Architecture:** A `storage` module constructs a `sqlx::SqlitePool` from hardened connection options and runs embedded migrations before returning. A focused `RepositoryStore` owns the pool and `RepositorySecretCipher`; it validates persisted encrypted values on reads and performs create, update, rename, rotation, and deletion in transactions while exposing metadata only.

**Tech Stack:** Rust 2021, Tokio, SQLx 0.8 with SQLite and migrations, AES-256-GCM security types, `thiserror`, and `tempfile` for integration tests.

## Global Constraints

- The schema must exactly match Specification 1's `repositories` table.
- Every connection must enable foreign keys, use WAL mode, and use a five-second busy timeout.
- Newly created Unix database files must have mode `0600`.
- Plaintext webhook secrets must never be bound to SQL or returned in metadata.
- Repository names remain authenticated encryption associated data; rename must decrypt and re-encrypt in one transaction.
- Canonical-name conflicts, missing IDs, cryptographic failures, unavailable/busy storage, migration failures, and internal persistence failures must remain distinguishable and redacted.

---

### Task 1: Hardened pool and embedded migration

**Files:**
- Create: `migrations/202608040001_create_repositories.sql`
- Create: `src/storage/mod.rs`
- Modify: `src/lib.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

**Interfaces:**
- Produces: `open_database(path: &Path) -> Result<SqlitePool, StorageError>`, `probe_database(pool: &SqlitePool) -> Result<(), StorageError>`, and `StorageError`.

- [ ] **Step 1: Write failing storage tests**

Add async tests that open a temporary file, inspect `sqlite_master`/`PRAGMA table_info`, query `PRAGMA foreign_keys`, `PRAGMA journal_mode`, and `PRAGMA busy_timeout`, and check Unix mode `0600`.

- [ ] **Step 2: Verify RED**

Run: `cargo test storage::tests`
Expected: compilation fails because `storage`, SQLx, and the migration do not exist.

- [ ] **Step 3: Add minimal migration and startup implementation**

Use these connection settings and run the static migrator before returning:

```rust
SqliteConnectOptions::new()
    .filename(path)
    .create_if_missing(true)
    .foreign_keys(true)
    .journal_mode(SqliteJournalMode::Wal)
    .busy_timeout(Duration::from_secs(5));
```

Securely create an absent Unix file with `OpenOptionsExt::mode(0o600)`, map migration errors separately, and implement the probe as `SELECT 1`.

- [ ] **Step 4: Verify GREEN**

Run: `cargo test storage::tests`
Expected: migration, pragma, probe, and Unix permission tests pass.

### Task 2: Repository domain metadata and transactional CRUD

**Files:**
- Create: `src/domain/mod.rs`
- Create: `src/domain/repository.rs`
- Create: `src/storage/repository_store.rs`
- Modify: `src/storage/mod.rs`
- Modify: `src/lib.rs`

**Interfaces:**
- Produces: `RepositoryId`, `RepositoryTimestamp`, `RepositoryMetadata`, `RepositoryMutation`, `RepositoryStore`, and `RepositoryStoreError`.
- Consumes: `CanonicalRepositoryName`, `RepositorySecret`, `EncryptedRepositorySecret`, `RepositorySecretCipher`, and `SqlitePool`.

- [ ] **Step 1: Write failing CRUD integration tests**

Specify the public API with tests equivalent to:

```rust
let created = store.create(name("Owner/Repo"), secret("value"), true).await?;
assert_eq!(created.full_name(), "owner/repo");
assert_eq!(store.get(created.id()).await?, created);
assert_eq!(store.list().await?, vec![created.clone()]);
store.delete(created.id()).await?;
assert_eq!(store.get(created.id()).await, Err(RepositoryStoreError::NotFound));
```

Also test duplicate names map to `Conflict`, enabled changes persist, metadata contains stable timestamps, and missing update/delete IDs return `NotFound`.

- [ ] **Step 2: Verify RED**

Run: `cargo test storage::repository_store::tests::crud`
Expected: compilation fails because domain values and `RepositoryStore` do not exist.

- [ ] **Step 3: Implement metadata and CRUD minimally**

Use private fields and documented accessors. Store timestamps as UTC SQLite text in a `RepositoryTimestamp` newtype. Convert rows with explicit `sqlx::Row::try_get`, reconstruct and authenticate encrypted fields before successful reads, and map SQLite extended result codes whose low byte is `5` or `6` to `Unavailable`.

- [ ] **Step 4: Verify GREEN**

Run: `cargo test storage::repository_store::tests::crud`
Expected: CRUD, conflict, not-found, and enabled-state tests pass.

### Task 3: Atomic rotation, rename, and persistence security

**Files:**
- Modify: `src/storage/repository_store.rs`

**Interfaces:**
- Consumes: `RepositoryMutation { full_name, webhook_secret, enabled }`.
- Produces: `RepositoryStore::update(id, mutation) -> Result<RepositoryMetadata, RepositoryStoreError>` with one-transaction rename/rotation semantics.

- [ ] **Step 1: Write failing atomicity and security tests**

Add tests proving repeated rotation changes nonce/ciphertext, plaintext does not appear in selected fields or database bytes, rename decrypts only with the new name, a conflicting rename leaves the original row unchanged/decryptable, and wrong-key/tampered rows cause cryptographic errors rather than successful partial metadata.

- [ ] **Step 2: Verify RED**

Run: `cargo test storage::repository_store::tests::encrypted`
Expected: tests fail because update does not yet perform rename/rotation encryption transitions.

- [ ] **Step 3: Implement the transactional update path**

Begin a SQL transaction, load and authenticate the current encrypted value, select the final canonical name, encrypt either the supplied secret or the decrypted current secret against that final name, update all requested fields and `updated_at`, and commit. Do not run a preflight uniqueness query; map SQLite's unique violation to `Conflict`.

- [ ] **Step 4: Verify GREEN**

Run: `cargo test storage::repository_store::tests::encrypted`
Expected: encryption, atomicity, plaintext-absence, wrong-key, and tamper tests pass.

### Task 4: Documentation and complete validation

**Files:**
- Create: `changelog/2026-08-04T09-40-41-0400-sqlite-repository-store.md`
- Modify: public API documentation as needed

**Interfaces:**
- Consumes: all APIs from Tasks 1-3.
- Produces: timestamped implementation record and warning-free documentation.

- [ ] **Step 1: Document the iteration**

Record migration behavior, connection hardening, encrypted CRUD, transactional rename/rotation, error categories, and test coverage without including secret values.

- [ ] **Step 2: Run mandatory gates in order**

Run: `just fmt && cargo clippy --all-targets -- -D warnings && just test`
Expected: all commands pass without warnings.

- [ ] **Step 3: Exercise issue-specific artifacts**

Run: `cargo build && cargo doc --no-deps`
Expected: the migrated-store implementation builds and public documentation completes without warnings; integration tests have exercised real temporary SQLite files.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock migrations src docs/superpowers/plans changelog
git commit -m "feat: add encrypted SQLite repository store

Closes #3"
```
