# Authenticated GitHub Webhook Ingestion Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Compose existing authentication, delivery-claim, and bounded-metrics foundations into `POST /webhooks/github`.

**Architecture:** A focused Axum webhook module validates request metadata and reads exact body bytes through a configured bounded extractor. It minimally deserializes repository identity, authenticates, validates optional action semantics, and then claims before recording typed bounded metrics for new deliveries. Request middleware records one normalized result and duration on every exit.

**Tech Stack:** Rust 2021, Axum 0.8, Tokio, Serde JSON, SQLx SQLite, existing HMAC and Prometheus components, Tower integration tests.

## Global Constraints

- Require exact JSON content type and the three GitHub webhook headers.
- Never buffer more than `GHE_WEBHOOK_BODY_LIMIT_BYTES` and authenticate the exact buffered bytes.
- Use untrusted repository identity only to locate authentication material.
- Authenticate before claiming, event/action normalization, or event/body metric updates.
- Return stable redacted `400`, `401`, `413`, `415`, and `503` responses.
- Unknown repositories, disabled repositories, and signature mismatches must return byte-identical `401` responses.
- Request count and duration include every request; event/body metrics include only authenticated new claims.
- Duplicate claims increment duplicate/request metrics but not event/body metrics.
- Never persist or log payloads, repository names, delivery IDs, signatures, secrets, ciphertext, or nonces.
- Keep retention scheduling and repository-gauge synchronization out of scope.
- Add a timestamped Markdown entry under `changelog/`.

---

### Task 1: Router contract and bounded request validation

**Files:**
- Create: `src/api/webhook.rs`
- Modify: `src/api/mod.rs`
- Modify: `src/app.rs`
- Test: `src/api/webhook.rs`

**Interfaces:**
- Produces: `webhook::router(body_limit_bytes: usize) -> Router<AppState>`
- Produces: `POST /webhooks/github`
- Consumes: an `AppState` body-limit accessor configured at startup

- [x] **Step 1: Write failing router tests**

Add table-driven requests through `build_router` for missing/malformed headers, non-JSON content type, malformed JSON, invalid repository identity, invalid delivery UUID, and an oversized body. Assert stable statuses and redacted response bodies, and assert oversized input creates no delivery row or event/body metric observation.

- [x] **Step 2: Run tests and verify RED**

Run: `cargo test api::webhook::tests -- --nocapture`

Expected: compilation fails because the webhook module and state configuration do not exist.

- [x] **Step 3: Add minimal validation and bounded body extraction**

Implement a focused async handler returning `Result<Response, AppError>`. Validate content type and header UTF-8/exact formats before deserializing only `repository.full_name`. Use `axum::extract::DefaultBodyLimit::max(body_limit_bytes)` and `Bytes` extraction so the same exact bytes feed JSON parsing and HMAC verification. Map body-limit rejection to `413` and other body rejection to `400`. Deserialize optional action semantics only after successful authentication and before claiming, so malformed unauthenticated action data cannot bypass HMAC verification and malformed authenticated data cannot consume a delivery claim.

- [x] **Step 4: Run tests and verify GREEN**

Run: `cargo test api::webhook::tests -- --nocapture`

Expected: validation/status tests pass.

### Task 2: Authentication, atomic claims, and bounded metrics

**Files:**
- Modify: `src/api/webhook.rs`
- Modify: `src/app.rs`
- Modify: `src/main.rs`
- Test: `src/api/webhook.rs`

**Interfaces:**
- Consumes: `WebhookAuthenticator`, `WebhookSignature`, `DeliveryStore`, `DeliveryClaim`, `Metrics`
- Produces: AppState access to a shared `DeliveryStore`
- Produces: one request result/duration observation on every webhook response

- [x] **Step 1: Write failing authenticated-flow tests**

Create enabled, disabled, and unknown repository fixtures. Generate HMAC signatures over exact fixture bytes. Assert enabled/correct requests return `204`; one-byte body or signature changes return `401`; disabled and unknown responses are byte-identical; and authentication database failure returns `503` without a claim or event metric.

- [x] **Step 2: Run authentication tests and verify RED**

Run: `cargo test api::webhook::tests::authentication -- --nocapture`

Expected: assertions fail because the handler does not authenticate.

- [x] **Step 3: Implement authentication mapping**

Parse `WebhookSignature` and authenticate the canonical repository against exact bytes. Map malformed signature syntax to the stable unauthorized response, `Unauthorized` to `401`, and `Unavailable` to `503` plus `FailureStage::Authentication`. Do not normalize event/action before success.

- [x] **Step 4: Run authentication tests and verify GREEN**

Run: `cargo test api::webhook::tests::authentication -- --nocapture`

Expected: authentication tests pass.

- [x] **Step 5: Write failing claim and metric tests**

Send the same authenticated delivery twice. Assert both return `204`, exactly one delivery UUID is persisted, event/body metrics increment once, duplicate metrics increment once, and request/duration metrics increment twice. Close SQLite before a fresh claim and assert `503`, `FailureStage::DeliveryClaim`, and no event/body update.

- [x] **Step 6: Run claim tests and verify RED**

Run: `cargo test api::webhook::tests::delivery -- --nocapture`

Expected: assertions fail because delivery claiming and metrics are not composed.

- [x] **Step 7: Implement claim ordering and metrics**

Authenticate, claim atomically, return accepted for duplicates after recording the duplicate metric, and normalize/observe event/action/body only for `DeliveryClaim::New`. Use a drop guard or single orchestration result mapping so request count and elapsed duration are recorded exactly once on every exit.

- [x] **Step 8: Run claim tests and verify GREEN**

Run: `cargo test api::webhook::tests::delivery -- --nocapture`

Expected: claim and metric tests pass.

### Task 3: Security regression coverage and documentation

**Files:**
- Modify: `src/api/webhook.rs`
- Create: `changelog/<timestamp>-authenticated-webhook-ingestion.md`

**Interfaces:**
- Verifies: stable response and exposition output contain no forbidden attacker-controlled or secret values

- [x] **Step 1: Write and run security-output tests**

Exercise malformed, unauthorized, duplicate, and unavailable paths with unique sensitive markers. Assert response status/headers/bodies and Prometheus exposition contain none of the repository name, delivery ID, signature, payload fragment, plaintext secret, ciphertext, or nonce markers.

Run: `cargo test api::webhook::tests::sensitive -- --nocapture`

Expected: tests pass only when every output remains redacted and labels remain bounded.

- [x] **Step 2: Add the timestamped changelog**

Document route behavior, processing order, safe errors, duplicate semantics, metrics behavior, and test coverage.

- [x] **Step 3: Run the full project gate from the top**

Run in order:

```bash
just fmt
cargo build
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps
```

Expected: every command exits zero without warnings.

- [x] **Step 4: Exercise the HTTP artifact**

Start the binary with a temporary migrated database and valid environment, configure one repository through the admin API, generate a signed payload, and `POST /webhooks/github`. Verify `204`, then resend and verify `204`; inspect `/metrics` for one event and one duplicate and inspect SQLite for only delivery UUID/timestamp persistence.

- [x] **Step 5: Commit the scoped change**

```bash
git add src docs/superpowers/plans changelog
git commit -m "feat: expose authenticated GitHub webhook ingestion

Closes #15"
```
