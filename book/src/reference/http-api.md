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

## `POST /ci-cost/usage`

Served only when `GHE_INGEST_OIDC_JSON` is set; otherwise the route is not registered and returns
`404 Not Found`. A GitHub Actions job reports spend that no webhook carries: model inference it has
already priced in USD, and Daytona sandbox time that the exporter prices from `costs.daytona`.

The caller authenticates with the OIDC token GitHub minted for the job, as
`Authorization: Bearer <token>`. Request it with the configured audience, for example
`core.getIDToken("<audience>")` from a job with `permissions: id-token: write`. The token must be
an RS256 JWT signed by a key in `https://token.actions.githubusercontent.com/.well-known/jwks`,
issued by `https://token.actions.githubusercontent.com`, for the configured audience, unexpired,
and for a repository that is both in `allowed_repositories` and registered through the admin API.
Keys are cached for an hour and refetched at most once a minute when a token names an unknown
`kid`. The token is never logged.

Attribution comes only from the verified claims: `repository`, `workflow`, `workflow_ref`,
`job_workflow_ref`, `run_id`, `run_attempt`, `event_name`, and `sha`. The pull request is the one
the run's stored `workflow_run` context is attributed to; when that delivery has not arrived yet,
the entry has no pull request.

The body is JSON, `Content-Type: application/json`, at most 64 KiB:

```json
{ "version": 1, "job": "run-evals", "entries": [
  { "source": "anthropic", "origin": "custom", "model": "<model>", "task": "<task>",
    "usd": 0.381, "input_tokens": 88400, "output_tokens": 9100 },
  { "source": "daytona", "task": "<task>", "cpu_seconds": 1840, "ram_gb_seconds": 7360 }
] }
```

| Field | Contract |
| --- | --- |
| `version` | Must be `1`. |
| `job` | Optional. The job's `GITHUB_JOB`. |
| `entries` | 1 to 100 entries. |
| `source` | 1 to 32 lowercase ASCII letters, digits, `_` or `-`. `daytona` marks sandbox usage; `github_actions` is reserved. |
| `usd` | Required for every non-`daytona` entry and forbidden on `daytona` entries. A finite number from 0 to 10,000. The exporter never prices tokens. |
| `model`, `origin` | Optional on inference entries. |
| `input_tokens`, `output_tokens` | Optional on inference entries; informational only. |
| `cpu_seconds`, `ram_gb_seconds` | Both required on `daytona` entries, forbidden on others. Finite, from 0 to 10^9. |
| `task` | Optional. |

Unknown fields fail, so a body can never supply attribution such as `repository` or `run_id`.
`job`, `task`, `model` and `origin` have control characters removed and keep at most 128
characters. A `daytona` entry is priced as `cpu_seconds × usd_per_cpu_second + ram_gb_seconds ×
usd_per_ram_gb_second`. Without Daytona rates it is recorded at 0 USD with
`ci.price.fell_back=true`, so the seconds are kept and a missing rate never fails the job.

| Status | `code` | Meaning |
| --- | --- | --- |
| `204 No Content` | | Every entry was stored, exported as one `ci.usage` span, and logged. |
| `400 Bad Request` | `schema_invalid` | Malformed JSON, an unknown or misplaced field, or an out-of-range value. |
| `401 Unauthorized` | `missing_token`, `invalid_token`, `token_expired`, `issuer_mismatch`, `audience_mismatch` | The token is absent or failed verification. |
| `403 Forbidden` | `repository_not_allowed`, `repository_not_registered` | The token's repository is not allowlisted, or not registered and enabled. |
| `413 Payload Too Large` | `body_too_large` | Body exceeds 64 KiB. |
| `415 Unsupported Media Type` | `unsupported_media_type` | Content type is not `application/json`. |
| `422 Unprocessable Entity` | `unsupported_version`, `missing_usd` | `version` is not `1`, or an inference entry has no `usd`. |
| `503 Service Unavailable` | `jwks_unavailable`, `storage_unavailable` | The signing keys or SQLite could not be reached. Retrying may succeed. |

A post is all or nothing: one invalid entry rejects the post, and nothing from it is stored. Every
rejection writes one parentless `ci usage post rejected` warning with the `code` as `reason`, plus
`github.repository` once the token has been verified. Accepted posts are not deduplicated, so a
client that retries after a timeout may record an entry twice.

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
