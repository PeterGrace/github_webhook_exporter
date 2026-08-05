# Repository Secret Security Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add misuse-resistant repository-secret encryption and administrator bearer-token authentication primitives.

**Architecture:** A `security` module owns validated secret-bearing newtypes, repository-name associated data, AES-256-GCM encryption, and bearer authentication. `RepositorySecretCipher` derives its key once with HKDF-SHA-256 and generates a fresh operating-system nonce for each encryption; `AdminAuthenticator` hashes both credentials and compares fixed-size digests in constant time.

**Tech Stack:** Rust 2021, `aes-gcm` 0.10, `hkdf` 0.12, `sha2` 0.10, `subtle` 2.6, `secrecy` 0.10, `thiserror` 2.

## Global Constraints

- Derive the version-1 key with context `github-webhook-exporter/repository-secret/v1`.
- Use fresh 96-bit operating-system CSPRNG nonces and canonical repository names as associated data.
- Reject empty secrets and secrets larger than 65,536 bytes.
- Secret-bearing values must zeroize and must not reveal or serialize plaintext.
- Missing, malformed, and incorrect bearer credentials must return one redacted failure.
- Persistence and HTTP middleware remain out of scope.

---

### Task 1: Validated security value types

**Files:**
- Create: `src/security/mod.rs`
- Modify: `src/lib.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

**Interfaces:**
- Produces: `MasterKey::from_slice`, `AdminToken::new`, `RepositorySecret::new`, `CanonicalRepositoryName::new`, `EncryptedRepositorySecret` accessors, and `SecurityError`.

- [ ] **Step 1: Write failing unit tests**

Add tests for key length, empty and 65,537-byte secrets, canonical repository-name normalization and validation, and redacted formatting.

- [ ] **Step 2: Verify RED**

Run: `cargo test security::tests`
Expected: compilation fails because `security` types do not exist.

- [ ] **Step 3: Implement minimal validated newtypes**

Use private fields, `SecretBox<[u8; 32]>` / `SecretString`, explicit constructors, no plaintext `Display` or serialization implementations, and safe encrypted-value accessors.

- [ ] **Step 4: Verify GREEN**

Run: `cargo test security::tests`
Expected: all value-type tests pass.

### Task 2: Version-1 repository-secret cipher

**Files:**
- Create: `src/security/secret_cipher.rs`
- Modify: `src/security/mod.rs`

**Interfaces:**
- Consumes: `MasterKey`, `RepositorySecret`, `CanonicalRepositoryName`, `EncryptedRepositorySecret`, and `SecurityError`.
- Produces: `RepositorySecretCipher::new`, `RepositorySecretCipher::encrypt`, and `RepositorySecretCipher::decrypt`.

- [ ] **Step 1: Write failing cipher tests**

Add tests for deterministic HKDF derivation, round trips, fresh nonces, wrong keys, changed ciphertext, changed nonce, changed associated data, and unsupported versions.

- [ ] **Step 2: Verify RED**

Run: `cargo test security::secret_cipher::tests`
Expected: compilation fails because `RepositorySecretCipher` does not exist.

- [ ] **Step 3: Implement minimal cipher**

Derive a 32-byte key with HKDF-SHA-256, encrypt with `Aes256Gcm`, generate `Nonce<U12>` with `OsRng`, bind canonical names as AAD, and map cryptographic failures to redacted typed errors.

- [ ] **Step 4: Verify GREEN**

Run: `cargo test security::secret_cipher::tests`
Expected: all cipher tests pass.

### Task 3: Constant-time administrator authentication

**Files:**
- Create: `src/security/admin_auth.rs`
- Modify: `src/security/mod.rs`

**Interfaces:**
- Consumes: `AdminToken`.
- Produces: `AdminAuthenticator::new`, `AdminAuthenticator::authenticate`, and `AuthenticationError`.

- [ ] **Step 1: Write failing authentication tests**

Add tests for exact, incorrect, truncated, extended, missing, malformed, and master-key-derived bearer values, plus redacted error formatting.

- [ ] **Step 2: Verify RED**

Run: `cargo test security::admin_auth::tests`
Expected: compilation fails because `AdminAuthenticator` does not exist.

- [ ] **Step 3: Implement minimal authenticator**

Parse only `Bearer <credential>`, hash expected and supplied bytes with SHA-256, compare fixed-size digests with `subtle::ConstantTimeEq`, and return the same unit error for every failure.

- [ ] **Step 4: Verify GREEN**

Run: `cargo test security::admin_auth::tests`
Expected: all authentication tests pass.

### Task 4: Documentation and full validation

**Files:**
- Create: `changelog/2026-08-04T08-40-14-0400-repository-secret-security.md`
- Modify: public API documentation as needed.

**Interfaces:**
- Consumes: all security APIs from Tasks 1-3.
- Produces: contributor-facing change record and validated public documentation.

- [ ] **Step 1: Add the timestamped changelog entry**

Document the key derivation, authenticated encryption, redacted newtypes, and bearer authentication behavior.

- [ ] **Step 2: Run mandatory gates in order**

Run: `just fmt && cargo clippy --all-targets -- -D warnings && just test`
Expected: all commands pass without warnings.

- [ ] **Step 3: Run issue-specific additional gates**

Run: `cargo build && cargo doc --no-deps`
Expected: both commands pass without warnings.

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock src docs/superpowers/plans changelog
git commit -m "feat: add repository secret security primitives

Closes #2"
```
