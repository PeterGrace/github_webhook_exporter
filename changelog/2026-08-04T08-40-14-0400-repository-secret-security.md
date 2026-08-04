# Repository secret security primitives

Date: 2026-08-04 08:40:14 -0400

## Added

- Validated, zeroizing types for master keys, administrator tokens, and plaintext repository secrets.
- Canonical repository-name validation for authenticated encryption associated data.
- Version-1 AES-256-GCM repository-secret encryption with HKDF-SHA-256 key derivation and fresh operating-system CSPRNG nonces.
- Storage-safe encrypted-secret values with redacted formatting and strict version, nonce, and ciphertext validation.
- Constant-time administrator bearer-token authentication using fixed-size SHA-256 digest comparison.
- Tests for cryptographic tampering, wrong keys, associated-data changes, secret boundaries, authentication failures, and redaction guarantees.

## Changed

- Runtime configuration now stores master keys and administrator credentials in focused security newtypes.
