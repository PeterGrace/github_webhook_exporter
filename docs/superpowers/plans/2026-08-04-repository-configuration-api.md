# Repository Configuration API Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** Expose authenticated Axum CRUD routes for encrypted repository configuration without returning or logging secret material.

**Architecture:** Initialize the migrated SQLite pool before serving and place an `AdminAuthenticator` and `RepositoryStore` in cloneable application state. A focused repository API module owns authentication extraction, strict request parsing, domain conversion, metadata response serialization, and stable HTTP error mapping while persistence and cryptography remain delegated to existing components.

**Tech Stack:** Rust 2021, Axum 0.8, Tokio, SQLx SQLite, Serde, Tower test utilities, tracing

## Global Constraints

- Every `/api/v1/repositories` route requires the independent `GHE_ADMIN_TOKEN` bearer credential.
- Missing, malformed, and invalid credentials return identical `401` responses with `WWW-Authenticate: Bearer`.
- Repository names are trimmed, ASCII-lowercased, and validated before persistence.
- Webhook secrets are accepted only on create or rotation and are never serialized or logged.
- Empty and oversized secrets, malformed names and IDs, malformed JSON, and empty PATCH objects return stable `400` JSON errors.
- Unknown IDs return `404`, canonical-name conflicts return `409`, and unexpected failures return a redacted `500`.
- PATCH applies rename, enablement, and secret rotation through one atomic store call.
- Handlers remain async and return `Result<Response, AppError>`.

---

### Task 1: Application state and authenticated routing foundation

**Files:**
- Modify: `src/app.rs`
- Modify: `src/main.rs`
- Modify: `src/lib.rs`
- Create: `src/api/mod.rs`
- Create: `src/api/repositories.rs`
- Test: `tests/repository_api.rs`

**Interfaces:**
- Consumes: `AdminAuthenticator::new(&AdminToken)`, `RepositorySecretCipher::new(&MasterKey)`, `RepositoryStore::new(SqlitePool, RepositorySecretCipher)`.
- Produces: `AppState::new(RepositoryStore, AdminAuthenticator) -> AppState`, `AppState::repository_store()`, `AppState::admin_authenticator()`, and `api::router() -> Router<AppState>`.

- [x] **Step 1: Write failing full-router authentication tests**

Create a migrated temporary SQLite pool and app state, then send missing, malformed, invalid, and valid bearer requests to `GET /api/v1/repositories`. Assert uniform `401` JSON plus `WWW-Authenticate: Bearer` for failures and `200 []` for the valid token.

- [x] **Step 2: Verify the authentication test fails for the missing routes**

Run: `cargo test --test repository_api authentication -- --nocapture`

Expected: FAIL because the router currently returns `404` and has no repository API.

- [x] **Step 3: Implement state initialization, API nesting, and admin extraction**

Open the database in `main` before binding/serving, construct the cipher/store/authenticator once, expose borrowed state accessors, and route all repository endpoints through an extractor that calls `AdminAuthenticator::authenticate`. Represent authentication failure with an `AppError` response carrying status `401`, code `unauthorized`, message `authentication required`, and the bearer challenge header.

- [x] **Step 4: Verify authentication tests pass**

Run: `cargo test --test repository_api authentication -- --nocapture`

Expected: PASS.

### Task 2: Create and read repository metadata

**Files:**
- Modify: `src/api/repositories.rs`
- Modify: `src/error.rs`
- Test: `tests/repository_api.rs`

**Interfaces:**
- Produces strict `CreateRepositoryRequest`, private metadata serialization, and handlers for create, list, and get.
- Maps `SecurityError` input failures to `400 invalid_request`, `RepositoryStoreError::NotFound` to `404 not_found`, and `RepositoryStoreError::Conflict` to `409 conflict`.

- [x] **Step 1: Write failing create/list/get tests**

Cover `201` creation, default and explicit `enabled`, canonicalized names, metadata-only response keys, ordered listing, fetch by ID, duplicate names, invalid names, empty and 65,537-byte secrets, malformed JSON, unknown IDs, zero IDs, negative IDs, and non-numeric IDs.

- [x] **Step 2: Verify create/read tests fail**

Run: `cargo test --test repository_api create -- --nocapture && cargo test --test repository_api get -- --nocapture && cargo test --test repository_api list -- --nocapture`

Expected: FAIL because CRUD handlers are not implemented.

- [x] **Step 3: Implement strict request conversion and create/read handlers**

Deserialize with `#[serde(deny_unknown_fields)]`, default `enabled` to true, move secret strings directly into `RepositorySecret`, canonicalize names through `CanonicalRepositoryName`, serialize only `id`, `full_name`, `enabled`, `created_at`, and `updated_at`, and normalize JSON/path extraction failures into `invalid_request`.

- [x] **Step 4: Verify create/read tests pass**

Run: `cargo test --test repository_api create -- --nocapture && cargo test --test repository_api get -- --nocapture && cargo test --test repository_api list -- --nocapture`

Expected: PASS.

### Task 3: Atomic PATCH and DELETE behavior

**Files:**
- Modify: `src/api/repositories.rs`
- Test: `tests/repository_api.rs`

**Interfaces:**
- Produces a strict optional-field `PatchRepositoryRequest`, conversion to `RepositoryMutation`, update handler, and delete handler.

- [x] **Step 1: Write failing PATCH and DELETE tests**

Cover each patch field, combined rename/enablement/rotation, empty objects, unknown fields, conflicts, unknown IDs, malformed IDs, successful `204` deletion with an empty body, and missing deletion targets.

- [x] **Step 2: Verify mutation tests fail**

Run: `cargo test --test repository_api patch -- --nocapture && cargo test --test repository_api delete -- --nocapture`

Expected: FAIL because mutation handlers are not implemented.

- [x] **Step 3: Implement update and delete handlers**

Build one `RepositoryMutation` from all supplied fields, reject an empty request before calling storage, delegate the combined mutation to `RepositoryStore::update`, and return an empty `204` response after `RepositoryStore::delete` succeeds.

- [x] **Step 4: Verify mutation tests pass**

Run: `cargo test --test repository_api patch -- --nocapture && cargo test --test repository_api delete -- --nocapture`

Expected: PASS.

### Task 4: Redaction, documentation, and project gates

**Files:**
- Modify: `src/error.rs`
- Modify: `tests/repository_api.rs`
- Create: `changelog/2026-08-04T10-30-34-0400-repository-configuration-api.md`

**Interfaces:**
- Produces redacted `AppError` formatting/response behavior and release documentation.

- [x] **Step 1: Write failing redaction assertions**

Assert success and error bodies omit plaintext secrets, ciphertext, nonce, encryption version, admin token, and master-key text. Exercise an internal storage failure and assert the public response remains exactly the stable `internal_error` envelope.

- [x] **Step 2: Verify redaction tests fail for any unsafe response or diagnostic**

Run: `cargo test --test repository_api redaction -- --nocapture`

Expected: FAIL until all response/error paths use metadata-only and fixed safe messages.

- [x] **Step 3: Harden error conversion and add the timestamped changelog**

Ensure `AppError` never formats sensitive sources into client responses or request logs, emits only normalized outcomes for internal failures, document every public API/state item, and summarize routes, validation, authentication, and tests in the changelog entry.

- [x] **Step 4: Run the mandatory validation sequence**

Run in order: `just fmt`, `cargo clippy --all-targets -- -D warnings`, `just test`.

Expected: all commands exit successfully without warnings.

- [x] **Step 5: Run the issue-specific acceptance gates**

Run: `cargo build && cargo doc --no-deps`

Expected: both commands exit successfully without warnings.

- [x] **Step 6: Commit the complete scoped change**

```bash
git add Cargo.lock Cargo.toml src tests docs/superpowers/plans/2026-08-04-repository-configuration-api.md changelog
git commit -m "feat: expose authenticated repository configuration API

Closes #4"
```
