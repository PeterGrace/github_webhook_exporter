# Service Operations

## Startup

The service validates configuration and initializes local structured stderr logging before opening
SQLite. It then opens the configured database, applies embedded migrations, constructs the router,
and binds the HTTP listener in that order.

A configuration, database-open, or migration failure is fatal. The process exits nonzero and does
not bind an HTTP listener, so it cannot report false readiness. Local errors identify the failed
stage without including configured credentials or the database path.

## Health endpoints

Both health routes are intentionally unauthenticated so an orchestrator can call them without an
administrator credential.

### `GET /health/live`

Returns `200 OK` while the HTTP process can serve requests. This handler does not access SQLite or
any external dependency. A database outage therefore does not change liveness.

```bash
curl --fail --silent --show-error http://127.0.0.1:8080/health/live
```

An empty successful response means the HTTP process is live.

### `GET /health/ready`

Runs a minimal `SELECT 1` probe against the migrated SQLite pool:

- `200 OK`: migrations completed during startup and SQLite currently accepts the probe.
- `503 Service Unavailable`: the probe failed.

```bash
curl --fail --silent --show-error http://127.0.0.1:8080/health/ready
```

Both responses have empty bodies. Database paths, SQL errors, and internal details are never sent
to the client. A readiness failure does not terminate the process; it emits a normalized structured
warning for local diagnosis.

## GitHub webhook endpoint

`POST /webhooks/github` is intentionally unauthenticated at the HTTP layer. GitHub authenticates
each request with the repository-specific HMAC secret configured through the administrator API.
The request must include:

- `Content-Type: application/json`
- a non-empty `X-GitHub-Event`
- `X-GitHub-Delivery` containing a UUID
- `X-Hub-Signature-256` containing `sha256=` and exactly 64 hexadecimal characters

The service buffers at most `GHE_WEBHOOK_BODY_LIMIT_BYTES` exact request bytes and initially reads
only `repository.full_name`. It verifies the HMAC before validating optional top-level `action`
semantics, then atomically claims the delivery UUID. A new authenticated delivery updates bounded
event/action and body-size metrics. An authenticated duplicate returns success and updates request
and duplicate metrics only. Payloads are never persisted.

| Status | Meaning |
| --- | --- |
| `204 No Content` | Authenticated new or duplicate delivery. |
| `400 Bad Request` | Missing/malformed headers, malformed JSON, invalid UUID, or invalid repository identity. |
| `401 Unauthorized` | Unknown/disabled repository or incorrect signature. |
| `413 Payload Too Large` | Body exceeds `GHE_WEBHOOK_BODY_LIMIT_BYTES`. |
| `415 Unsupported Media Type` | Content type is not exactly `application/json`. |
| `503 Service Unavailable` | SQLite could not load authentication data or claim the delivery. |

Unknown, disabled, and incorrectly signed requests return identical `401` response status, headers,
and body. Every error body is fixed and excludes repository names, delivery IDs, signatures,
payload fragments, and storage details. Structured logs contain only normalized result/stage values
and an opaque correlation ID for internal dependency failures.

## Graceful shutdown

Tokio listens for both SIGINT and SIGTERM. Either signal follows the same sequence:

1. Record the normalized signal in structured stderr logging.
2. Stop accepting new connections.
3. Allow active requests to finish for at most `GHE_SHUTDOWN_TIMEOUT_SECONDS`.
4. Exit normally if all requests complete.
5. Drop remaining connections and record a normalized timeout warning if the duration expires.

The timeout defaults to 30 seconds and must be a positive integer. For example, to request a
five-second drain:

```bash
GHE_SHUTDOWN_TIMEOUT_SECONDS=5 github_webhook_exporter
```

Service managers should send SIGTERM and configure their own termination grace period to exceed
this timeout. SIGINT is primarily useful for an interactive local run.

Lifecycle logs contain the bound address, normalized signal kind, completion, and timeout duration.
They do not log runtime configuration, authorization headers, request bodies, encryption material,
or repository secrets.
