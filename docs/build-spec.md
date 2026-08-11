# Build Spec: GitHub Webhook Exporter

> **Historical draft. Not a reference for the running service.** This monolithic draft has been
> superseded by the reviewed decomposition in
> [`docs/superpowers/specs/2026-08-03-github-webhook-exporter-design.md`](superpowers/specs/2026-08-03-github-webhook-exporter-design.md).
> The scoped specifications govern wherever they conflict with this document.
>
> It is kept because the changelog and the superseding design cite it. Parts of it were never built
> as described, and the text below is deliberately left as written rather than corrected, with
> divergences from the implementation flagged inline. For current behavior see
> [`README.md`](../README.md) and the
> [documentation site](https://petergrace.github.io/github_webhook_exporter/).

## Objective

Build a single-instance Rust daemon that:

- Authenticates GitHub webhook deliveries for multiple repositories.
- Exposes bounded-cardinality Prometheus metrics.
- Tracks merge-group and per-pull-request merge-queue outcomes.
- Exports structured logs and traces using OTLP/HTTP.
- Stores encrypted repository secrets and derived queue state in SQLite.
- Never persists complete webhook payloads.

## Architecture

```text
GitHub
   |
   v
POST /webhooks/github
   |
   +-- enforce 2 MiB limit
   +-- parse repository.full_name
   +-- load encrypted secret from SQLite
   +-- verify X-Hub-Signature-256 against original bytes
   +-- classify and update metrics
   +-- persist minimal merge-queue state
   +-- discard payload
   |
   v
204 No Content

Prometheus ------> GET /metrics
Administrators --> /api/v1/repositories
OTel Collector <--- OTLP/HTTP logs and traces
```

The Kubernetes deployment will use a single-replica StatefulSet with a PVC containing SQLite.

## HTTP API

### Webhook endpoint

`POST /webhooks/github`

Required headers:

- `Content-Type: application/json`
- `X-GitHub-Event`
- `X-GitHub-Delivery`
- `X-Hub-Signature-256`

Responses:

| Status | Meaning |
| --- | --- |
| `204` | Valid delivery accepted, including duplicate deliveries |
| `400` | Malformed JSON or missing required headers |
| `401` | Unknown repository or invalid signature |
| `413` | Payload exceeds configured limit |
| `415` | Unsupported content type |
| `503` | Configuration database unavailable, so authentication could not be performed |

After signature validation, metric or queue-state failures do not change the `204` response. They
produce an error log, error trace event, and internal failure metric.

### Configuration API

> **Diverged:** the implementation authenticates these routes with a separate `GHE_ADMIN_TOKEN`
> bearer token, not the master key. The master key is used only to derive the encryption key.

All routes require:

```http
Authorization: Bearer <master-key>
```

Routes:

| Method | Route | Purpose |
| --- | --- | --- |
| `POST` | `/api/v1/repositories` | Register a repository and webhook secret |
| `GET` | `/api/v1/repositories` | List configured repositories without secrets |
| `GET` | `/api/v1/repositories/{id}` | Return repository metadata |
| `PATCH` | `/api/v1/repositories/{id}` | Rename, enable, disable, or rotate its secret |
| `DELETE` | `/api/v1/repositories/{id}` | Remove configuration |

Repository names are canonicalized to lowercase `owner/repository`.

Secrets are accepted only on create or rotation and are never returned by the API.

## Security

### Master key

`GHE_MASTER_KEY` contains a base64-encoded, cryptographically random 32-byte key.

It is used for:

1. Constant-time configuration API authentication.
2. Deriving a separate database encryption key through HKDF-SHA-256.

> **Diverged:** only the second use was built. Configuration API authentication uses
> `GHE_ADMIN_TOKEN`.

Repository webhook secrets are encrypted with an authenticated encryption algorithm such as
AES-256-GCM. Each encrypted value receives a fresh random nonce.

Other controls:

- Do not log payloads, signatures, webhook secrets, master keys, or API request bodies.
- Restrict configuration routes with Kubernetes NetworkPolicy or ingress rules.
- Terminate TLS at the ingress or service mesh.
- Apply request-body limits before JSON parsing.
- Verify signatures against the exact original request bytes.
- Never use repository data from an unverified request except to locate the candidate secret.

## SQLite Data Model

### `repositories`

```text
id
full_name                 UNIQUE, canonical lowercase
webhook_secret_ciphertext
webhook_secret_nonce
enabled
created_at
updated_at
```

### `merge_queue_attempts`

```text
id
repository_id
pull_request_number
enqueued_at
completed_at              NULL while active
outcome                   pending/succeeded/failed/cancelled/unknown
reason_code
```

Only derived identifiers and state are stored. No complete payload, commit message, author, email,
or webhook secret is retained here.

### `processed_deliveries`

```text
delivery_id               PRIMARY KEY
received_at
```

This provides idempotency for GitHub redeliveries. Old entries should be removed periodically, with
a configurable default retention of seven days.

SQLite should use WAL mode, foreign keys, busy timeouts, and embedded migrations. Database startup
or migration failures prevent readiness.

## Event Processing

### Generic events

After authentication:

1. Read the event type from `X-GitHub-Event`.
2. Read `action` from the JSON payload when present.
3. Normalize missing or unrecognized actions to `none` or `other`.
4. Update generic event metrics.
5. Dispatch recognized event types to specialized processors.

Event types remain open-ended. Repository names, delivery IDs, pull request numbers, SHAs, and other
unbounded values are never Prometheus labels.

### Merge groups

Process:

- `merge_group.checks_requested`
- `merge_group.destroyed`

For destroyed groups, normalize `reason` to:

- `merged`
- `dequeued`
- `invalidated`
- `other`

A `merged` merge group is a reliable group-level success. A dequeued group is not automatically
classified as a CI failure.

### Pull-request merge-queue attempts

Process these `pull_request` actions:

- `enqueued`: open a pending attempt.
- `dequeued`: complete the active attempt using a bounded reason classifier.
- `closed` with `pull_request.merged == true`: complete an active attempt as succeeded.

Outcomes:

- `succeeded`
- `failed`
- `cancelled`
- `unknown`

GitHub does not document a fixed enumeration for `pull_request.dequeued.reason`. Therefore:

- Known failure reasons may map to `failed`.
- Clearly user-initiated removal may map to `cancelled`.
- Unrecognized reasons map to `unknown`.
- Raw reason strings must not become metric labels.

This limitation should be documented on the dashboard: group-level `merged` statistics are
authoritative, while per-pull-request failure classification is initially best-effort.

## Prometheus Metrics

> **Incomplete:** the service registers fifteen instruments. This list predates the workflow-job
> and telemetry-health metrics. See the metrics table in [`README.md`](../README.md#metrics).

```text
github_webhook_requests_total{repository,result}
github_webhook_events_total{repository,event_type,action}
github_webhook_processing_duration_seconds{repository,result}
github_webhook_request_body_bytes{repository}

github_merge_group_events_total{repository,action,reason}
github_merge_queue_pr_outcomes_total{repository,outcome,reason}
github_merge_queue_attempt_duration_seconds{repository,outcome}

github_webhook_duplicates_total{repository}
github_webhook_processing_failures_total{repository,stage}
github_repository_configurations
```

Bounded labels:

- `result`: `accepted`, `malformed`, `unauthorized`, `too_large`, `unsupported`, or `unavailable`.
- `stage`: `metrics`, `queue_state`, `database`, or `telemetry`.
  **Diverged:** the implemented values are `authentication`, `delivery_claim`, `metrics`,
  `database`, and `queue_state`; there is no `telemetry` stage.
- Merge-group actions and reasons use fixed enumerations.
- Generic unknown actions collapse to `other`.
- Authenticated repository identity is the canonical lowercase `owner/repository` label; requests
  that fail before authentication use the fixed value `unknown`.

## OpenTelemetry

Use OTLP/HTTP for both traces and logs.

Standard configuration should be honored where possible:

```text
OTEL_EXPORTER_OTLP_ENDPOINT
OTEL_EXPORTER_OTLP_HEADERS
OTEL_EXPORTER_OTLP_TIMEOUT
OTEL_SERVICE_NAME=github-webhook-exporter
RUST_LOG
```

Telemetry behavior:

- Use batched, bounded exporters.
- Continue serving if the collector is unavailable.
- Report exporter saturation or dropped telemetry loudly through local stderr logging.
- Flush exporters during graceful shutdown with a bounded timeout.
- Include service version, pod name, and Kubernetes namespace as resource attributes.
- Do not attach payloads, secrets, authorization headers, or unvalidated identifiers to spans.
  Authenticated webhook spans use the canonical repository name.
- A delivery ID may be included in trace context but never as a metric label.

Primary spans:

```text
http.request
github.webhook.authenticate
github.webhook.process
merge_queue.update
config.repository.write
sqlite.query
```

> **Incomplete:** the implementation also emits `retention.run`, and `github.workflow.job` with
> `github.workflow.step` children for completed workflow jobs.

## Kubernetes Operation

- Single-replica StatefulSet.
- PVC mounted for SQLite.
- Configurable database path via `GHE_DATABASE_PATH`.
- Kubernetes Secret supplies `GHE_MASTER_KEY` and optional OTLP headers.
  **Diverged:** the Secret must also supply `GHE_ADMIN_TOKEN`; startup fails without it.
- Graceful SIGTERM handling.
- Pod disruption budget is optional for the singleton.
- Use an update strategy that avoids concurrent pods mounting and writing the database.

Endpoints:

| Route | Purpose |
| --- | --- |
| `/health/live` | Process and Tokio runtime are alive |
| `/health/ready` | SQLite is reachable and migrations succeeded |
| `/metrics` | Prometheus scrape endpoint |

OTel collector availability must not affect readiness.

## Suggested Rust Components

- `axum`, `tower`, and `tower-http`
- `tokio`
- `serde` and `serde_json`
- `sqlx` with SQLite and embedded migrations
- `hmac`, `sha2`, and `subtle`
- `aes-gcm`, `hkdf`, `secrecy`, and `zeroize`
- `prometheus-client`
- `tracing` and `tracing-subscriber`
- `opentelemetry` and `opentelemetry_sdk`
- `opentelemetry-otlp`
- `tracing-opentelemetry`
- `opentelemetry-appender-tracing`
- `thiserror` and `anyhow`

The current `console-subscriber` and `tokio_unstable` configuration should be development-only or
removed from production builds.

## Acceptance Criteria

- A correctly signed configured-repository webhook receives `204`.
- Invalid signatures and unknown repositories receive `401`.
- Payloads over 2 MiB receive `413`.
- Webhook secrets are not readable as plaintext from SQLite.
- Configuration API responses never expose stored secrets.
- Duplicate delivery IDs do not update event or merge-queue outcome metrics twice.
- `merge_group` events expose `checks_requested` and destroyed-reason statistics.
- Enqueued pull requests can be correlated with later queue outcomes across process restarts.
- Authenticated canonical repository names label repository-scoped metrics; pull request numbers,
  SHAs, and delivery IDs never appear as metric labels.
- Webhook processing continues when the OTel collector is unavailable.
- Payload and secret data do not appear in logs or traces.
- The pod becomes unready when SQLite cannot be opened.
- Graceful shutdown drains HTTP requests and flushes telemetry within a fixed timeout.

## Open Questions

- Exact `pull_request.dequeued.reason` values need to be observed before the failure classifier can
  be made comprehensive.
- Retention duration for completed merge-queue attempts is TBD; 90 days is a reasonable initial
  default.
- Whether a future API or dashboard should expose aggregate SQLite history, beyond Prometheus
  metrics, remains out of scope for v1.
