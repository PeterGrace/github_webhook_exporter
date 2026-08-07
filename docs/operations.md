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

## Remote telemetry

Structured stderr logging is always active. Remote trace and log export is optional and starts only
when `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, or
`OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` is set. The generic HTTP endpoint receives `/v1/traces` and
`/v1/logs` automatically; a signal-specific endpoint is used exactly as configured.

The OTLP/HTTP protobuf exporters honor generic and signal-specific endpoint, header, and timeout
variables. `OTEL_EXPORTER_OTLP_TIMEOUT` and its signal-specific variants are milliseconds. Header
values are validated but redacted from errors and debug output. `OTEL_SERVICE_NAME` defaults to
`github-webhook-exporter`; every resource includes the package version. Of the values supplied in
`OTEL_RESOURCE_ATTRIBUTES`, only `k8s.pod.name` and `k8s.namespace.name` are retained.

Each enabled signal uses a non-blocking bounded queue. These application settings accept positive
integers:

| Variable | Default | Contract |
| --- | ---: | --- |
| `GHE_OTEL_QUEUE_CAPACITY` | `2048` | Maximum admitted records per signal. |
| `GHE_OTEL_BATCH_SIZE` | `512` | Maximum records per request; no greater than queue capacity. |
| `GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS` | `5` | Reserved for #36; not yet enforced. |

Invalid requested telemetry configuration fails startup with only the variable name. Collector
latency or unavailability occurs on dedicated exporter threads and does not change HTTP readiness
or request results. During this interim phase, drop and export-failure counts are internal hooks
only: they are not yet exposed through Prometheus or direct stderr diagnostics, so collector
failure can silently discard remote telemetry. Issue
[#35](https://github.com/PeterGrace/github_webhook_exporter/issues/35) adds those operator-visible
counters and diagnostics. Final graceful provider shutdown and application-wide spans are also
delivered by later Phase 4 work.

## Exported core traces

The core trace vocabulary contains six stable service operations:

- `http.request`: one root for each HTTP request.
- `github.webhook.authenticate`: repository lookup and HMAC authentication under the request root.
- `github.webhook.process`: payload projection, durable delivery claim, and bounded event processing
  under an authenticated request.
- `config.repository.write`: create, update, or delete persistence under an administrator API
  request.
- `sqlite.query`: one child for each logical repository, delivery, or merge-queue store operation.
- `merge_queue.update`: one specialized merge-group or pull-request transition under webhook
  processing.

Each scheduled delivery and merge-queue pruning pass emits an independent `retention.run` root.
Its `delivery.prune` and `merge_queue.prune` SQLite operations are children of that root, even when
the retention task is started from a context that contains another span.

`http.route` contains the Axum route template, such as `/api/v1/repositories/{id}`. Unmatched
requests use the fixed value `unmatched`. Raw paths, query strings, and URLs are not trace
attributes. HTTP methods, response classes, repository operations, webhook event/action values,
database operations, merge-group reasons, queue outcomes/reasons, terminal outcomes, and failure
events use closed bounded vocabularies.

Repository names and database identifiers, delivery identifiers, pull-request numbers, valid full
commit SHAs, workflow identifiers, and sanitized workflow names are diagnostic trace span
attributes only. They are excluded from structured stderr, OTLP application logs, and Prometheus
exposition.

Duplicate delivery claims emit an authentication span and a process span with outcome `duplicate`,
but no second `merge_queue.update` span or specialized outcome. Queue-state persistence failures
retain the authenticated `204 No Content` response and emit only error status plus an
`operation.failure` event with bounded reason `queue_state`. A merge-group `destroyed` event with
reason `dequeued` records normalized group reason `dequeued`; a pull-request dequeue records outcome
`unknown` and reason `unclassified_dequeue`. Retention roots report only `success`, `cancelled`, or
`failure` and contain no cutoff or row identifiers.

Trace attributes and events never include request bodies or payload fragments, repository secrets,
webhook signatures, authorization or OTLP headers, actors, raw URLs, commands, raw actions or
reasons, SQL statements, database paths, or internal error text.

### Completed workflow traces

Only an authenticated `workflow_job` webhook whose normalized action is `completed` can emit a
historical workflow trace. Admission occurs only after the durable delivery claim returns `New`, so
a duplicate, unsupported action, or malformed specialized projection emits no workflow spans.
Admission is at most once: after a delivery claim commits, the service does not retry projection or
emission for that delivery, including after a process interruption. Generic webhook responses and
metrics retain their normal behavior.

Each accepted projection creates an independent root named `github.workflow.job`, with a new trace
identity unrelated to the live `http.request` trace. Every projected step is a direct child named
`github.workflow.step`; payload-provided names never become span names. The service does not create
a workflow-run root, persist workflow data, correlate jobs across deliveries, or mutate merge-queue
state for a workflow-job event.

A job uses its exact RFC 3339 `started_at` and `completed_at` values only when both parse and start is
not after completion; this is marked `timing_source=reported`. Otherwise it is instantaneous at a
valid completion time, or at the request receipt time when completion is missing or malformed, and
is marked `timing_source=fallback`. A step uses reported timing only when both values parse, are
ordered, and lie inside the selected job interval. Every other step interval is instantaneous at
the selected job end and marked as fallback.

Conclusions are normalized to `success`, `failure`, `cancelled`, `skipped`, `timed_out`, `neutral`,
or `other`. The CI/CD result is respectively `success`, `failure`, `cancellation`, `skip`, or
`timeout` where that semantic value exists; it is omitted for `neutral` and `other`. `success` sets
OpenTelemetry status OK, `failure` and `timed_out` set error status with a fixed description, and
all other conclusions leave status unset. Raw unknown conclusions are discarded.

The workflow root may contain only these validated span-only identifiers: canonical repository
name, delivery UUID, workflow run ID, positive run attempt, workflow job ID, valid full head SHA,
and at most the first 20 positive pull-request numbers. The run ID is exported under both
`cicd.pipeline.run.id` and `github.workflow.run.id`; each step's
`cicd.pipeline.task.run.id` is derived as `<job-id>:<positive-step-number>`. Workflow, job, and step
display names are span-only. Sanitization removes all Unicode control characters, retains at most
the first 128 remaining Unicode scalar values, and omits an empty result.

Commands, output, logs, actors or other user data, raw or derived URLs, request bodies and arbitrary
payload fragments, secrets, signatures, authorization and other headers, unsupported actions, and
raw unknown conclusions are not represented in workflow telemetry. They do not enter OTLP traces,
OTLP logs, structured stderr, or Prometheus exposition. Approved identifiers and sanitized names
enter spans only.

Historical spans use the existing bounded non-blocking trace queue. The request path does not wait
for export or retry collector failures synchronously. Queue drops and export failures are accounted
for by the telemetry runtime but never change the authenticated `204 No Content` response,
readiness, generic webhook metrics, or merge-queue state. Collector endpoints and exporter error
details are not exposed in responses or application logs.

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

### Merge-group statistics

A newly claimed `merge_group.checks_requested` delivery increments
`github_merge_group_events_total{action="checks_requested",reason="none"}`. A newly claimed
`merge_group.destroyed` delivery uses action `destroyed` and maps the top-level `reason` exactly and
case-sensitively to `merged`, `dequeued`, or `invalidated`; missing, non-string, mixed-case, and
unknown values map to `other`. Unsupported merge-group actions update only the generic webhook
event metric. Duplicate deliveries update only the webhook request and duplicate counters, so no
generic or specialized event is counted twice during uninterrupted operation.

Group-level `destroyed` with reason `merged` is the authoritative merge-group success statistic.
These group metrics remain intentionally separate from per-pull-request queue attempt outcomes. A
merge group can contain multiple pull requests, and webhook ordering does not provide a reliable
join, so merge-group deliveries never create or mutate pull-request attempt rows. Raw reasons,
repository names, group identifiers, and head SHAs are discarded rather than logged or used as
metric labels.

### Pull-request merge-queue attempts

For a newly claimed `pull_request` delivery, the authenticated repository's durable identifier and
positive `pull_request.number` select one attempt. `enqueued` creates a pending attempt only when
none is active. `dequeued` completes an active attempt as `unknown/unclassified_dequeue`;
`closed` completes it as `succeeded/pull_request_merged` only when `pull_request.merged` is exactly
`true`. An absent or false merged flag and unsupported actions do not mutate specialized state.
Raw dequeue reasons are discarded and are never persisted, logged, or used as metric labels.

The processor uses a valid `pull_request.updated_at` for the transition timestamp. Missing,
malformed, or unrepresentable timestamps fall back to the receipt time captured once for the
request. Repeated enqueue and already-completed replay are no-ops. A completion with no active or
completed attempt increments only
`github_merge_queue_transition_failures_total{reason="missing_active_attempt"}`. Outcome and
duration metrics update only after SQLite commits a pending-to-completed transition. Negative and
over-365-day durations are omitted and increment only the bounded `invalid_duration` transition
failure.

Queue processing begins after the delivery claim commits. A queue-state persistence failure
therefore still returns `204 No Content`, increments
`github_webhook_processing_failures_total{stage="queue_state"}`, and emits one redacted local error
with an opaque correlation ID. GitHub is not asked to redeliver the already claimed delivery: queue
processing is intentionally at-most-once at this boundary. SQLite state and in-memory Prometheus
metrics are also not one transaction; a crash after queue state commits but before metrics update
can undercount an outcome until the process restarts, and Phase 3 does not claim exactly-once
metrics across crashes.

## Delivery and merge-queue retention

The service retains authenticated delivery claims for `GHE_DELIVERY_RETENTION_DAYS` (default: 7)
and completed pull-request merge-queue attempts for `GHE_MERGE_QUEUE_RETENTION_DAYS` (default:
90). Both values must be positive integers. Pending merge-queue attempts are retained regardless of
age so a later completion can correlate across a restart.

One background task starts both pruning workloads every
`GHE_DELIVERY_PRUNE_INTERVAL_SECONDS` (default: 3600); Phase 3 intentionally has no separate queue
prune interval. There is no immediate startup pass, and missed ticks are skipped rather than
replayed in a burst. Each scheduled pass fixes its cutoffs once, then each SQLite operation deletes
at most 1,000 eligible rows. A workload repeats bounded operations until one deletes fewer than
1,000 rows. Delivery pruning preserves fresh claims. Queue pruning deletes only attempts with a
non-null completion time older than its cutoff, preserving fresh completed and all pending
attempts.

A database failure stops only the affected workload's current pass. Normalized structured logging
identifies the workload and outcome and includes an opaque correlation ID for failures; it excludes
SQL text, row identifiers, repository identities, pull-request numbers, timestamps, and payload
data. The next configured interval retries both workloads.

During uninterrupted operation, a repeated delivery UUID is accepted but changes only request and
duplicate metrics. Queue state and specialized metrics update at most once after the durable
delivery-claim boundary. A process crash after a delivery or queue-state commit but before the
corresponding in-memory metric update can undercount it. SQLite and Prometheus are not
transactionally coupled, so the service explicitly does not promise exactly-once metrics across
crashes.

## Graceful shutdown

Tokio listens for both SIGINT and SIGTERM. Either signal follows the same sequence:

1. Record the normalized signal in structured stderr logging.
2. Notify both Axum and the shared retention task through one cancellation signal.
3. Stop accepting new connections and stop scheduling new delivery or queue prune batches.
4. Allow active requests and active SQLite prune work to finish within one shared
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
