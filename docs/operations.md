# Service Operations

## Startup

The service validates configuration and initializes local structured stderr logging before opening
SQLite. It then opens the configured database, applies embedded migrations, constructs the router,
and binds the HTTP listener in that order.

A configuration, database-open, migration, or initial repository-count failure is fatal. Before
binding, the service initializes `github_repository_configurations` from the durable repository
count, so readiness is never served with a default gauge value. Successful API creates and deletes
adjust the in-memory gauge after their SQLite commits. A forced process exit in that narrow gap can
leave the final in-process value stale, but the next startup reconciles it from durable state. The
process exits nonzero on any startup failure and cannot report false readiness. Local errors
identify the failed stage without including configured credentials or the database path.

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
and body. Every error body is fixed and excludes repository names, delivery IDs, signatures, payload
fragments, and storage details. Internal/database failures add an opaque UUID `error_id`; the same
ID appears in the corresponding local structured log for diagnosis. It is generated locally, does
not encode request data, and is never a metric label. Structured logs otherwise contain only
normalized result/stage values.

## Delivery retention and duplicate semantics

The service retains authenticated delivery claims for `GHE_DELIVERY_RETENTION_DAYS` (default: 7).
One background task starts a pruning pass every `GHE_DELIVERY_PRUNE_INTERVAL_SECONDS` (default:
3600). A pass uses a fixed cutoff and deletes at most 1,000 expired claims per SQLite operation,
repeating until a batch deletes fewer than 1,000. Fresh claims are preserved. Failures end only the
current pass and are reported with normalized structured fields; the next configured interval can
retry.

During uninterrupted operation, a repeated delivery UUID is accepted but changes only request and
duplicate metrics. A process crash after the durable claim commits and before the in-memory event
counter changes can undercount that delivery. SQLite and Prometheus are not transactionally
coupled, so the service explicitly does not promise exactly-once metrics across crashes.

## Graceful shutdown

Tokio listens for both SIGINT and SIGTERM. Either signal follows the same sequence:

1. Record the normalized signal in structured stderr logging.
2. Notify both Axum and the delivery-retention task through one cancellation signal.
3. Stop accepting new connections and stop scheduling new prune batches.
4. Allow active requests and an active SQLite prune batch to finish within one shared
   `GHE_SHUTDOWN_TIMEOUT_SECONDS` deadline.
5. Exit normally if all lifecycle work completes.
6. Drop remaining work and record a normalized timeout warning if the shared deadline expires.

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
