# HTTP API

## `GET /health/live`

Unauthenticated. Returns `200 OK` with an empty body while the HTTP process can serve requests.
This handler does not access SQLite or any external dependency — a database outage does not change
liveness.

## `GET /health/ready`

Unauthenticated. Runs a minimal `SELECT 1` probe against the migrated SQLite pool.

| Status | Meaning |
| --- | --- |
| `200 OK` | Migrations completed during startup and SQLite currently accepts the probe. |
| `503 Service Unavailable` | The probe failed. |

Both responses have empty bodies. Database paths, SQL errors, and internal details are never sent
to the client. A readiness failure does not terminate the process; it emits a normalized structured
warning for local diagnosis.

## `GET /metrics`

Unauthenticated. OpenMetrics exposition for Prometheus. See [Metrics](metrics.md) for every
series.

## `POST /webhooks/github`

Intentionally unauthenticated at the HTTP layer — GitHub authenticates each request with the
repository-specific HMAC secret configured through the admin API. Required:

- `Content-Type: application/json`
- non-empty `X-GitHub-Event`
- `X-GitHub-Delivery` containing a UUID
- `X-Hub-Signature-256` containing `sha256=` followed by exactly 64 hexadecimal characters

The service buffers at most `GHE_WEBHOOK_BODY_LIMIT_BYTES` exact request bytes, reads
`repository.full_name`, verifies the HMAC over the exact bytes before validating optional
top-level `action` semantics, then atomically claims the delivery UUID. Payloads are never
persisted.

| Status | Meaning |
| --- | --- |
| `204 No Content` | Authenticated new or duplicate delivery. |
| `400 Bad Request` | Missing/malformed headers, malformed JSON, invalid UUID, or invalid repository identity. |
| `401 Unauthorized` | Unknown/disabled repository or incorrect signature. |
| `413 Payload Too Large` | Body exceeds `GHE_WEBHOOK_BODY_LIMIT_BYTES`. |
| `415 Unsupported Media Type` | Content type is not exactly `application/json`. |
| `503 Service Unavailable` | SQLite could not load authentication data or claim the delivery. |

Unknown, disabled, and incorrectly signed requests return an identical `401` response — status,
headers, and body — so a caller cannot distinguish them. Every error body is fixed and excludes
repository names, delivery IDs, signatures, payload fragments, and storage details. Internal or
database failures add an opaque `error_id`; the same ID appears in the corresponding local
structured log. It is generated locally, encodes no request data, and is never a metric label.

Requests that fail before authentication are recorded with the fixed repository value `unknown`,
so an unauthenticated payload cannot create arbitrary metric series. Successfully authenticated
requests use the canonical lowercase `owner/repository` form — `PeterGrace/github-webhook-exporter`
normalizes to `petergrace/github-webhook-exporter`, never to `github-webhook-exporter` alone.

## Admin API

Requires `Authorization: Bearer <GHE_ADMIN_TOKEN>`. Never returns webhook secrets, including to the
administrator who set them.

| Method and path | Purpose |
| --- | --- |
| `POST /api/v1/repositories` | Register a repository and its webhook secret. |
| `GET /api/v1/repositories` | List configured repositories. |
| `GET /api/v1/repositories/{id}` | Fetch repository metadata. |
| `PATCH /api/v1/repositories/{id}` | Rename, enable, disable, or rotate the secret. |
| `DELETE /api/v1/repositories/{id}` | Remove the configuration. |

`POST /api/v1/repositories` request body:

```json
{"full_name": "owner/repository", "webhook_secret": "the-secret-you-set-on-github"}
```
