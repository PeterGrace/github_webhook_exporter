# Specification 1: Service and Repository Configuration

## Goal

Provide a secure, operable HTTP service and an authenticated API for managing encrypted GitHub
repository webhook secrets in SQLite. This specification does not accept GitHub webhooks or export
Prometheus event metrics.

## Dependencies

None.

## Runtime configuration

| Variable | Requirement |
| --- | --- |
| `GHE_DATABASE_PATH` | Required path to the SQLite database file. |
| `GHE_MASTER_KEY` | Required base64 encoding of exactly 32 random bytes. |
| `GHE_ADMIN_TOKEN` | Required independent, high-entropy bearer token. |
| `GHE_BIND_ADDRESS` | Optional socket address; default `0.0.0.0:8080`. |
| `RUST_LOG` | Optional tracing filter; default `info`. |
| `GHE_SHUTDOWN_TIMEOUT_SECONDS` | Optional positive integer; default `30`. |

Invalid required configuration prevents startup and produces a redacted stderr error.

## HTTP service

The Axum service provides:

| Method | Route | Authentication | Success |
| --- | --- | --- | --- |
| `GET` | `/health/live` | None | `200` while the process can serve requests. |
| `GET` | `/health/ready` | None | `200` after migrations and a successful database probe. |
| `POST` | `/api/v1/repositories` | Bearer token | `201` with metadata. |
| `GET` | `/api/v1/repositories` | Bearer token | `200` with a metadata array. |
| `GET` | `/api/v1/repositories/{id}` | Bearer token | `200` with metadata. |
| `PATCH` | `/api/v1/repositories/{id}` | Bearer token | `200` with updated metadata. |
| `DELETE` | `/api/v1/repositories/{id}` | Bearer token | `204`. |

Missing or invalid credentials return `401` and a `WWW-Authenticate: Bearer` header. Authorization
uses constant-time comparison. API request bodies and authorization headers are never logged.

## Repository representation

Requests use JSON. A create request is:

```json
{
  "full_name": "owner/repository",
  "webhook_secret": "secret supplied to GitHub",
  "enabled": true
}
```

`enabled` defaults to `true`. A PATCH request may contain any non-empty combination of:

```json
{
  "full_name": "new-owner/new-repository",
  "webhook_secret": "rotated secret",
  "enabled": false
}
```

Responses contain only:

```json
{
  "id": 1,
  "full_name": "owner/repository",
  "enabled": true,
  "created_at": "2026-08-03T20:00:00Z",
  "updated_at": "2026-08-03T20:00:00Z"
}
```

Repository names are trimmed, converted to ASCII lowercase, and must contain exactly one `/`.
Owner and repository segments must each contain 1–100 ASCII alphanumeric, hyphen, underscore, or
period characters. Empty webhook secrets and secrets over 64 KiB are rejected. Unknown IDs return
`404`; duplicate canonical names return `409`; malformed requests return `400`.

## Cryptography

- Decode `GHE_MASTER_KEY` and require exactly 32 bytes.
- Derive an AES-256-GCM key using HKDF-SHA-256.
- Use the context string `github-webhook-exporter/repository-secret/v1`.
- Generate a fresh 96-bit nonce from the operating system CSPRNG for every create or rotation.
- Authenticate the canonical repository name as associated data, preventing ciphertext from being
  moved between repository records unnoticed.
- Store encryption format version `1` with each ciphertext.
- Keep plaintext secrets in secret-bearing, zeroizing memory and never implement `Debug` or
  serialization for plaintext-secret types.

Renaming a repository decrypts and re-encrypts its secret in the same transaction because the
canonical name is associated data.

## SQLite model

```sql
CREATE TABLE repositories (
    id INTEGER PRIMARY KEY,
    full_name TEXT NOT NULL UNIQUE,
    webhook_secret_ciphertext BLOB NOT NULL,
    webhook_secret_nonce BLOB NOT NULL,
    encryption_version INTEGER NOT NULL,
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
```

Embedded migrations run before the server becomes ready. Every connection enables foreign keys,
uses a finite five-second busy timeout, and participates in WAL mode. Repository mutations are
transactions. The database file is created without group or world access on Unix; deployments are
also responsible for restrictive volume permissions.

## Lifecycle and errors

Startup failure to open or migrate SQLite terminates the process. A database probe failure changes
readiness to `503` but does not terminate the process. SIGTERM and SIGINT stop accepting new
connections, drain in-flight requests up to the configured timeout, and then exit.

Expected client failures use structured JSON error bodies with a stable `code` and human-readable
`message`. Internal details are logged locally with secrets redacted and return code
`internal_error` with status `500`.

## Tests

- Configuration parsing rejects absent, malformed, and wrong-length secrets.
- Admin authentication accepts only the exact token and emits no credential data.
- Canonicalization and validation cover valid names, invalid separators, invalid characters, and
  length boundaries.
- Encryption round trips; distinct writes use distinct nonces; wrong keys, modified ciphertext,
  modified associated data, and unsupported versions fail closed.
- CRUD integration tests cover metadata-only responses, conflicts, atomic rotation, rename
  re-encryption, empty PATCH requests, and missing IDs.
- Captured logs and serialized responses contain no admin token, master key, plaintext repository
  secret, ciphertext, or nonce.
- Health tests cover migration completion and database probe failure.
- Shutdown tests prove in-flight requests receive the configured drain interval.

## Acceptance criteria

- The process refuses invalid security configuration.
- All repository secrets are authenticated ciphertext at rest.
- The master key is never accepted by or returned from HTTP endpoints.
- Configuration API responses never expose secret material.
- Repository writes are atomic and canonical-name uniqueness is enforced.
- Liveness, readiness, structured stderr logging, and bounded graceful shutdown work as specified.
