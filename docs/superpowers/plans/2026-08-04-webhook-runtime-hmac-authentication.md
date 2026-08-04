# Webhook Runtime and HMAC Authentication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add validated Phase 2 runtime settings and a narrow, secret-safe API for authenticating exact GitHub webhook request bytes for enabled repositories.

**Architecture:** `RuntimeConfig` validates and owns the three webhook settings. `RepositoryStore` performs one enabled-only lookup and decrypts the selected secret, while `security::webhook_auth` parses a typed signature and performs HMAC-SHA-256 verification over borrowed bytes. Public errors and debug output remain stable and redacted.

**Tech Stack:** Rust 2021, `hmac` 0.12, `hex` 0.4, `sha2` 0.10, `secrecy` 0.10, `sqlx` 0.8, `thiserror` 2.

## Global Constraints

- `GHE_WEBHOOK_BODY_LIMIT_BYTES` is positive, defaults to `2097152`, and may not exceed `2097152`.
- `GHE_DELIVERY_RETENTION_DAYS` is positive and defaults to `7`.
- `GHE_DELIVERY_PRUNE_INTERVAL_SECONDS` is positive and defaults to `3600`.
- Signatures must be exactly `sha256=` plus 64 hexadecimal characters.
- HMAC-SHA-256 must cover the exact original request bytes and use constant-time verification.
- Only enabled canonical repositories may yield candidate secrets; unknown and disabled repositories share one redacted result.
- Secret-bearing and attacker-controlled values must not appear in errors, debug output, logs, or serialization.
- HTTP routing, delivery persistence, metrics, and semantic event processing remain out of scope.

---

### Task 1: Phase 2 runtime configuration

**Files:**
- Modify: `src/config.rs`

**Interfaces:**
- Produces: `RuntimeConfig::webhook_body_limit_bytes() -> usize`, `RuntimeConfig::delivery_retention() -> Duration`, and `RuntimeConfig::delivery_prune_interval() -> Duration`.

- [ ] **Step 1: Write failing configuration tests**

Extend the default and override tests with the exact values above. Extend the invalid-value table with zero, malformed, and an over-limit body value, asserting rendered errors contain the variable name but not the supplied value.

- [ ] **Step 2: Run the tests to verify RED**

Run: `cargo test config::tests`
Expected: compilation fails because the new accessors do not exist.

- [ ] **Step 3: Implement minimal validation**

Add private fields and constants, parse each optional value through one positive-integer helper, reject body limits over `2097152`, and expose the validated values through documented accessors.

- [ ] **Step 4: Run the tests to verify GREEN**

Run: `cargo test config::tests`
Expected: all configuration tests pass.

### Task 2: Enabled repository authentication lookup

**Files:**
- Modify: `src/storage/repository_store.rs`
- Modify: `tests/storage.rs`

**Interfaces:**
- Consumes: `&CanonicalRepositoryName`.
- Produces: `RepositoryStore::authentication_secret(&CanonicalRepositoryName) -> Result<RepositorySecret, RepositoryStoreError>` and `RepositoryStoreError::AuthenticationFailed`.

- [ ] **Step 1: Write failing storage tests**

Create enabled, disabled, and absent repository cases. Assert the enabled lookup returns usable secret material and both disabled and absent cases return the same `AuthenticationFailed` variant and rendered text. Add a tampered encrypted row case and scan errors/debug output for repository names, plaintext, ciphertext, and nonce markers.

- [ ] **Step 2: Run the tests to verify RED**

Run: `cargo test --test storage authentication`
Expected: compilation fails because `authentication_secret` and `AuthenticationFailed` do not exist.

- [ ] **Step 3: Implement minimal lookup**

Select only encryption columns from `repositories WHERE full_name = ? AND enabled = 1`, map no row to `AuthenticationFailed`, reconstruct the encrypted value, and decrypt it with the queried canonical name. Preserve redacted database and cryptographic failures.

- [ ] **Step 4: Run the tests to verify GREEN**

Run: `cargo test --test storage authentication`
Expected: all authentication lookup tests pass.

### Task 3: Typed GitHub signature parsing and HMAC authentication

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Create: `src/security/webhook_auth.rs`
- Modify: `src/security/mod.rs`

**Interfaces:**
- Produces: `WebhookSignature::parse(&str) -> Result<WebhookSignature, WebhookAuthenticationError>`, `WebhookAuthenticator::new(&RepositoryStore)`, and `WebhookAuthenticator::authenticate(&CanonicalRepositoryName, &WebhookSignature, &[u8]) -> Result<(), WebhookAuthenticationError>`.

- [ ] **Step 1: Write failing parser and authenticator tests**

Test exact lowercase prefix handling, exact digest length, upper/lower hexadecimal digits, non-hex input, truncation, and extension. Test a known GitHub-compatible HMAC fixture, a one-byte payload change, wrong secret, malformed signature, and modified digest. Assert all unauthorized cases render identically and debug output contains none of the repository, signature, payload, or secret markers.

- [ ] **Step 2: Run the tests to verify RED**

Run: `cargo test security::webhook_auth::tests`
Expected: compilation fails because the webhook authentication types do not exist.

- [ ] **Step 3: Implement minimal parser and authenticator**

Decode exactly 32 digest bytes into a private array, redact `Debug`, load the enabled secret through `RepositoryStore`, initialize `Hmac<Sha256>` from secret bytes, and call `verify_slice` over the borrowed body. Map absent/disabled/bad signatures to one unauthorized error while retaining one redacted unavailable/internal category for future HTTP `503` mapping.

- [ ] **Step 4: Run the tests to verify GREEN**

Run: `cargo test security::webhook_auth::tests`
Expected: all webhook authentication tests pass.

### Task 4: Documentation and validation

**Files:**
- Create: `changelog/2026-08-04T18-06-42Z-webhook-runtime-hmac-authentication.md`
- Modify: public API documentation where required.

**Interfaces:**
- Consumes: all APIs from Tasks 1-3.
- Produces: a timestamped change record and warning-free public documentation.

- [ ] **Step 1: Add the timestamped changelog entry**

Document validated settings, enabled-only secret lookup, exact signature parsing, constant-time HMAC verification, and redaction behavior.

- [ ] **Step 2: Run mandatory gates in order**

Run: `just fmt`
Run: `cargo build`
Run: `cargo clippy --all-targets -- -D warnings`
Run: `just test`
Run: `cargo doc --no-deps`
Expected: every command passes without warnings.

- [ ] **Step 3: Commit**

```bash
git add Cargo.toml Cargo.lock src tests docs/superpowers/plans changelog
git commit -m "feat: add webhook HMAC authentication

Closes #12"
```
