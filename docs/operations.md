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

## Production container image

The supported production image uses a digest-pinned Rust builder and a distroless Debian runtime.
This iteration supports `linux/amd64` only. It runs the application directly as PID 1 under the
fixed non-root identity `65532:65532`, exposes only TCP port 8080, and contains no shell or Rust
build tooling.

Build the default `github-webhook-exporter:dev` tag locally:

```bash
just image-build
```

Set `CONTAINER_IMAGE` when a release or registry-specific tag is required. The same value selects
the image exercised by the smoke verification:

```bash
CONTAINER_IMAGE=registry.example/github-webhook-exporter:0.1.0 just image-build
CONTAINER_IMAGE=registry.example/github-webhook-exporter:0.1.0 just image-smoke
```

The image working and data directory is `/var/lib/github-webhook-exporter`. Mount persistent
storage at that path and set `GHE_DATABASE_PATH` to a file within it, such as
`/var/lib/github-webhook-exporter/github-webhook-exporter.db`. An empty Docker volume inherits the
directory's ownership from the image. Other volume providers must make the mount writable by UID
and GID 65532.

Every container requires these environment variables:

| Variable | Contract |
| --- | --- |
| `GHE_DATABASE_PATH` | Writable SQLite file path, normally below the mounted data directory. |
| `GHE_MASTER_KEY` | Base64 encoding of exactly 32 random bytes, supplied from a secret store. |
| `GHE_ADMIN_TOKEN` | Non-empty administrator bearer token, supplied from a secret store. |

Do not place secret values in image arguments, labels, Dockerfiles, or committed manifests. The
optional `GHE_*`, `OTEL_*`, and `RUST_LOG` variables retain the contracts documented in the
sections below and in startup validation.

The image deliberately defines no Docker health check. Kubernetes and other orchestrators should
use `GET /health/live` and `GET /health/ready` as described below. Because the binary is the direct
entrypoint, SIGTERM reaches the application's graceful lifecycle without a shell intermediary. Set
the orchestrator termination grace period greater than the sum of
`GHE_SHUTDOWN_TIMEOUT_SECONDS` and `GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS` so both application work and
telemetry providers receive their full shutdown boundaries.

### GHCR releases

Pull requests and `main` are validation-only: they build and smoke-test the production image,
validate the packaged chart, and never authenticate to GHCR or publish a package. Their temporary
chart artifacts are retained for 30 days through workflow artifacts.

A stable `vMAJOR.MINOR.PATCH` repository tag publishes one immutable `linux/amd64` image and one
Helm OCI chart only after full validation passes. The release workflow requires the tag without
`v`, the Cargo package version, the Helm chart version, and the Helm `appVersion` to match exactly
before it authenticates.

For example, after all four version fields are `0.1.0`, create and push the release tag, then
consume the published image and chart:

```bash
git tag v0.1.0
git push origin v0.1.0
docker pull ghcr.io/petergrace/github-webhook-exporter:0.1.0
helm pull oci://ghcr.io/petergrace/charts/github-webhook-exporter --version 0.1.0
helm install github-webhook-exporter oci://ghcr.io/petergrace/charts/github-webhook-exporter --version 0.1.0
```

Published version tags are immutable. Existing image tags are never overwritten. An exact matching
existing image permits chart-only recovery only when the chart is absent. The workflow never
publishes `latest`, branch, SHA, or prerelease tags. The overwrite guard is not atomic with the
registry push; repository administrators must prevent concurrent or manual pushes to release tags
because GHCR does not enforce this repository policy. If validation fails, rerun the original failed
workflow attempt without moving the tag. Only the image-existing/chart-missing state with an exact
matching digest may resume as chart-only recovery. Completed, chart-only, and digest-conflict states
fail closed without overwrite.

#### Image and chart state matrix

| Image state | Chart state | Result |
| --- | --- | --- |
| missing | missing | Publish the immutable image first, then the chart after validation. |
| matching digest | missing | Chart-only recovery; rerun the original failed workflow attempt without moving the tag after confirming the remote image configuration digest exactly matches the rebuilt image. |
| different digest | missing | Fail closed; digest-conflict state fails closed without overwrite. |
| missing | present | Fail closed; chart-only registry state fails closed without overwrite. |
| matching digest | present | Completed; fail closed without overwrite. |
| different digest | present | Fail closed; digest-conflict state fails closed without overwrite. |

The only resumable state is image-existing/chart-missing with an exact digest match. Any completed
publication, chart-only registry state, or digest conflict must not be overwritten.

## Helm deployment

The supported singleton Helm package is documented in the
[chart README](../charts/github-webhook-exporter/README.md). It installs exactly one StatefulSet
replica with a `ReadWriteOnce` PVC for SQLite, a ClusterIP Service, and fixed non-root UID/GID and
`fsGroup` 65532 security settings. Storage providers that ignore `fsGroup` must provision a mount
writable by that identity.

The chart maps typed non-secret values through a ConfigMap and references one operator-created
Secret for `GHE_MASTER_KEY`, `GHE_ADMIN_TOKEN`, and any configured OTLP header keys. It never
creates a Secret. `service.port` authoritatively configures the Service, container, named probes,
and IPv6 wildcard listener. Liveness remains process-only, readiness checks SQLite, and collector
availability affects neither probe.

An empty `image.tag` selects the chart `appVersion`; the default repository is
`ghcr.io/petergrace/github-webhook-exporter`. Stable release tags publish the matching immutable
image version, so the chart default resolves without an image override. Operators may still set
`image.repository` and `image.tag` when using a separately published compatible image.

The pod termination grace period must be strictly greater than the sum of the application and
telemetry shutdown timeouts. The chart adds no `preStop` delay. A deterministic ConfigMap checksum
in the pod template causes ConfigMap-backed value changes to trigger the StatefulSet's
`RollingUpdate`, ensuring replacement pods read updated environment values. This rollout does not
by itself guarantee Recreate-equivalent handoff or prevent writer overlap on every storage
provider; use the stopped upgrade procedure below when attachment overlap is possible.

## Upgrade a singleton deployment

A normal one-replica StatefulSet `RollingUpdate` terminates ordinal `0` before creating its
replacement. Use it only when the storage provider reliably completes volume handoff before a
replacement process can mount the PVC:

```bash
: "${GHE_IMAGE_TAG:?set GHE_IMAGE_TAG to an immutable published version}"
helm upgrade github-webhook-exporter charts/github-webhook-exporter \
  --namespace github-webhook-exporter \
  --reuse-values \
  --set-string image.tag="${GHE_IMAGE_TAG}" \
  --wait
kubectl --namespace github-webhook-exporter rollout status \
  statefulset/github-webhook-exporter --timeout=180s
```

When a storage provider can permit attachment overlap during transitions, use a
Recreate-equivalent stopped upgrade. This procedure intentionally causes downtime:

```bash
: "${GHE_IMAGE_TAG:?set GHE_IMAGE_TAG to an immutable published version}"
kubectl --namespace github-webhook-exporter scale \
  statefulset/github-webhook-exporter --replicas=0
kubectl --namespace github-webhook-exporter wait --for=delete \
  pod/github-webhook-exporter-0 --timeout=180s

helm upgrade github-webhook-exporter charts/github-webhook-exporter \
  --namespace github-webhook-exporter \
  --reuse-values \
  --set maintenanceMode=true \
  --set-string image.tag="${GHE_IMAGE_TAG}" \
  --wait

helm upgrade github-webhook-exporter charts/github-webhook-exporter \
  --namespace github-webhook-exporter \
  --reuse-values \
  --set maintenanceMode=false \
  --wait
kubectl --namespace github-webhook-exporter wait --for=condition=Ready \
  pod/github-webhook-exporter-0 --timeout=180s
```

`maintenanceMode=true` records the upgraded Helm release with zero desired replicas, so Helm does
not undo the stopped maintenance window. Disabling it restores exactly one replica. Confirm the old
pod is absent before applying the maintenance-mode upgrade; if volume detachment is asynchronous,
also wait for the provider's attachment object or console to report release.

The optional `minAvailable: 0` PodDisruptionBudget intentionally permits voluntary disruption. It
cannot preserve availability for a singleton, and maintenance mode is not a high-availability
mechanism.

## Back up SQLite online

Run the repository's pinned maintenance command while the exporter is available. It mounts the PVC
in a separate non-root Pod, uses SQLite's online `.backup` operation, validates the resulting
database, and sets mode `0600`:

```bash
backup_name="backup-$(date -u +%Y%m%dT%H%M%SZ).db"
scripts/helm-sqlite-maintenance.sh backup \
  github-webhook-exporter \
  github-webhook-exporter \
  data-github-webhook-exporter-0 \
  "${backup_name}"
```

The command uses the current kubectl context. Set `KUBECONFIG` and `KUBECTL_CONTEXT` when operating
against a non-default context. The backup file initially resides on the application PVC. Copy it to
an encrypted, access-controlled backup system or take a provider snapshot after the command
completes. A backup retained only on the application PVC does not protect against PVC loss.

Do not copy the active `github-webhook-exporter.db` file by itself. SQLite may have committed state
in its WAL, so a live file copy is unsupported and may be inconsistent. The production image
intentionally contains no shell or SQLite client; use the digest-pinned maintenance Pod or a
storage-platform snapshot workflow that provides equivalent consistency.

## Restore and verify a backup

Ensure the selected backup is present on the target PVC, then stop the exporter through Helm and
wait for its pod to disappear:

```bash
: "${BACKUP_NAME:?set BACKUP_NAME to the validated .db backup basename}"
helm upgrade github-webhook-exporter charts/github-webhook-exporter \
  --namespace github-webhook-exporter \
  --reuse-values \
  --set maintenanceMode=true \
  --wait
kubectl --namespace github-webhook-exporter wait --for=delete \
  pod/github-webhook-exporter-0 --timeout=180s

scripts/helm-sqlite-maintenance.sh restore \
  github-webhook-exporter \
  github-webhook-exporter \
  data-github-webhook-exporter-0 \
  "${BACKUP_NAME}"
```

Restore fails before creating a maintenance Pod unless desired replicas equal zero and ordinal `0`
is absent. It validates the backup and restored database, creates the replacement as UID/GID
`65532:65532`, verifies mode `0600`, removes stale WAL/shared-memory files, and retains the replaced
database as `github-webhook-exporter.db.pre-restore`. Do not delete that file or the source backup
until recovery is accepted.

Restart exactly one replica and verify recovery:

```bash
helm upgrade github-webhook-exporter charts/github-webhook-exporter \
  --namespace github-webhook-exporter \
  --reuse-values \
  --set maintenanceMode=false \
  --wait
kubectl --namespace github-webhook-exporter wait --for=condition=Ready \
  pod/github-webhook-exporter-0 --timeout=180s
kubectl --namespace github-webhook-exporter port-forward \
  service/github-webhook-exporter 8080:8080
```

In a separate shell, require all of these checks before accepting the recovery:

1. `GET /health/ready` returns `200`, proving startup migrations and SQLite probing completed.
2. `GET /api/v1/repositories` lists the expected encrypted repository configuration.
3. A correctly signed webhook using a pre-backup repository secret returns `204`.
4. `/metrics` exposes the repository gauge and expected bounded webhook families.
5. Replaying a pre-backup delivery ID increments the duplicate counter instead of processing twice.
6. A pre-backup pending merge-queue attempt can complete after recovery.

Use runtime-provided administrator and webhook credentials for those checks; never place them in a
command history, manifest, values file, or diagnostic artifact.

If the maintenance command reports a metadata failure, keep maintenance mode enabled and correct
the storage provider's UID/GID 65532 and permission behavior before retrying. If readiness fails,
inspect normalized pod logs and PVC events without printing the database or Secret. If repository
decryption fails, verify that the original master-key Secret was restored; a database backup alone
cannot recover a lost encryption key.

Helm rollback can restore rendered configuration, but it does not reverse SQLite migrations,
storage-template changes, or an incompatible application downgrade. Recover those cases from a
validated pre-upgrade backup with a compatible image. Do not treat `helm rollback` as database
rollback.

## Helm package validation and maintenance

Prerequisites: `just`, `helm`, `docker`, `kubeconform`, `conftest`, and `yq` on PATH. The pinned
CI install script keeps those versions aligned with the workflow contract.

Run these focused checks from the repository root after installing the pinned CI tools:

```bash
just helm-static
just image-smoke
just workflow-test
just helm-maintenance-unit
just helm-kind-acceptance
KIND_ARTIFACT_DIRECTORY=dist/kind-lifecycle just helm-kind-lifecycle
```

`just helm-static` validates chart metadata, rendering, schema, policy, secret, and packaged
archive contracts across the supported Kubernetes range 1.31.0 through 1.35.0
(`>=1.31.0-0 <1.36.0-0`). `just image-smoke` builds and exercises the production image locally.
`just workflow-test` checks the GitHub Actions contract, including the exact archive path
`dist/github-webhook-exporter-0.1.0.tgz`. `just helm-kind-acceptance` confirms API acceptance for the
rendered StatefulSet, Service, ConfigMap, and PVC; it does not start the exporter.

`just helm-kind-lifecycle` builds and loads the `linux/amd64` production image into a uniquely named
disposable Kind cluster. It creates private test credentials at runtime, installs the chart, and
proves probes, repository administration, signed webhooks, bounded metrics, SQLite persistence,
delivery deduplication, pull-request queue completion across restart, collector-failure isolation,
broken-storage readiness, bounded SIGTERM, singleton PVC rollout behavior, online backup, stopped
restore, restored metadata, and post-recovery encrypted repository, deduplication, queue, and metric
behavior. Its default
diagnostics directory is `dist/kind-lifecycle`; set `KIND_ARTIFACT_DIRECTORY` to isolate concurrent
runs. The suite replaces that directory at startup and scans rendered objects, status records,
events, descriptions, and logs for generated credentials, signatures, and forbidden payload
material before success.

The cluster and private temporary files are removed after both success and failure. For interactive
debugging only, `KEEP_KIND_CLUSTER=true` preserves them and prints the generated cluster name and
kubeconfig path; delete that cluster and temporary directory manually as soon as investigation
finishes. CI uses checksum-pinned Kind 0.31.0 and kubectl 1.35.0, while the harness pins the Kind
Kubernetes 1.35.0 node image by digest. CI uploads the diagnostics for 14 days with `if: always()`.

For local archive inspection, use the fixed package name directly:

```bash
helm show chart dist/github-webhook-exporter-0.1.0.tgz
helm show values dist/github-webhook-exporter-0.1.0.tgz
helm template archive dist/github-webhook-exporter-0.1.0.tgz --kube-version 1.35.0 >/dev/null
```

If `just helm-policy` fails, run `just helm-render` first, then inspect the rendered manifests under
`dist/rendered/` and compare them with the bounded policy fixtures under
`ci/helm/negative/policy/`. If `just helm-secrets` fails,
inspect the rendered manifests and ensure the chart never copies credentials into ConfigMaps,
Services, Ingresses, ServiceMonitors, or NetworkPolicies. The chart README documents the exact
Secret contract for runtime credentials.

When updating CI tools, edit `ci/tool-versions.env` and the corresponding checksums together, then
re-run `scripts/install-ci-tools.sh` or `just workflow-test` to confirm the pinned downloads still
validate. Do not rely on runner-provided tools or silently accept checksum drift.

Static validation only covers packaging, render, schema, policy, and smoke-test contracts.
passing static checks does not prove cluster lifecycle behavior.
Runtime readiness and rollout remain a cluster responsibility.

## Remote telemetry

Structured stderr logging is always active. Remote trace and log export is optional and starts only
when `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, or
`OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` is set. With none of these variables, no remote provider or
export queue is created and local logging remains fully functional. The generic HTTP endpoint
receives `/v1/traces` and `/v1/logs` automatically; a signal-specific endpoint is used exactly as
configured.

The OTLP/HTTP protobuf exporters honor generic and signal-specific endpoint, header, and timeout
variables. `OTEL_EXPORTER_OTLP_TIMEOUT` and its signal-specific variants are milliseconds. Header
values are percent-decoded, validated, and redacted from errors and debug output. An explicitly
empty signal-specific header variable clears inherited generic headers. Read an encoded header list
from the operator's shell environment rather than placing it in a script:

```bash
: "${OTEL_EXPORTER_OTLP_HEADERS:?set OTEL_EXPORTER_OTLP_HEADERS in the operator shell}"
export OTEL_EXPORTER_OTLP_HEADERS
OTEL_EXPORTER_OTLP_ENDPOINT=https://collector.example.test:4318 \
OTEL_EXPORTER_OTLP_TIMEOUT=10000 \
github_webhook_exporter
```

For independently routed signals, set `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` and
`OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` to their complete endpoints, including `/v1/traces` and
`/v1/logs`. `OTEL_SERVICE_NAME` defaults to `github-webhook-exporter`; every resource includes the
package version. Of the values supplied in `OTEL_RESOURCE_ATTRIBUTES`, only `k8s.pod.name` and
`k8s.namespace.name` are retained.

Each enabled signal uses a non-blocking bounded queue. These application settings accept positive
integers:

| Variable | Default | Contract |
| --- | ---: | --- |
| `GHE_OTEL_QUEUE_CAPACITY` | `2048` | Maximum admitted records per signal. |
| `GHE_OTEL_BATCH_SIZE` | `512` | Maximum records per request; no greater than queue capacity. |
| `GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS` | `5` | One shared trace-and-log shutdown deadline. |

Invalid requested telemetry configuration fails startup with only the variable name. Collector
latency or unavailability occurs on dedicated exporter threads and does not change HTTP readiness
or request results.

Pipeline failures and rejected records are exposed through these local Prometheus families:

- `github_telemetry_export_failures_total{signal,reason}`
- `github_telemetry_dropped_records_total{signal,reason}`

`signal` is exactly `trace` or `log`. Export-failure `reason` is one of `transport`, `timeout`,
`http_response`, `encoding`, `shutdown`, `internal`, or `other`. Drop `reason` is `queue_full` or
`pipeline_closed`. All bounded series exist at zero before the first event. A malformed successful
collector response is an `encoding` failure; collector response bodies and transport error text are
never exposed.

Each counter update may also produce a direct stderr line containing only the diagnostic kind,
signal, reason, and numeric suppression count. This path bypasses `tracing` and OpenTelemetry logs,
so exporter diagnostics cannot recursively enter the failing log pipeline. Output is limited to one
line per signal/reason category per monotonic minute. Repeated events still increment Prometheus;
the next permitted line reports how many local lines were suppressed.

Alert on sustained increases in
`github_telemetry_export_failures_total{reason=~"transport|timeout|http_response"}` because they
indicate collector reachability, latency, or response failures. `encoding` and `internal` should be
investigated as compatibility or implementation faults; unexpected `shutdown` indicates lifecycle
misordering. A rising `github_telemetry_dropped_records_total{reason="queue_full"}` is primarily a
capacity/tuning signal under load, while `pipeline_closed` before planned shutdown indicates
incorrect producer lifecycle ordering.

On shutdown, application admission closes before provider workers disconnect. Accepted records
that export release their pending slots; any slots still pending when shutdown finishes or reaches
its deadline are atomically counted as `pipeline_closed` drops. A finalization gate prevents a
late shutdown worker from exporting a batch after its slots were counted as dropped. Later records
are also rejected and counted exactly as `pipeline_closed`. Trace and log provider shutdown begins
concurrently and shares one
`GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS` deadline. A failed provider is counted with normalized reason
`shutdown`; a provider unfinished at the deadline is counted with reason `timeout`. Either condition
uses the same direct, redacted stderr diagnostic path and never changes a successful HTTP drain into
a process failure.

Repository, delivery, pull-request, commit, workflow, job, and step identifiers remain span-only.
Allowing these identifiers in local or OTLP application logs is a deferred policy choice and is not
current behavior.

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
attributes only, except for the bounded workflow-job rejection warning documented in
[Completed workflow traces](#completed-workflow-traces). Outside that narrow exception, they are
excluded from structured stderr, OTLP application logs, and Prometheus exposition.

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
a duplicate, unsupported action, or malformed specialized projection emits no workflow spans. A
zero or negative step number rejects the entire specialized projection; it never produces a partial
root-and-children trace. Admission is at most once: after a delivery claim commits, the service does
not retry projection or emission for that delivery, including after a process interruption. Generic
webhook responses and metrics retain their normal behavior.

`GHE_WORKFLOW_JOB_MAX_STEPS` bounds completed workflow-job admission. It defaults to `256`, accepts
only integer values in `1..=1024`, and has no unlimited mode. The inclusive limit applies after the
delivery claim and after the bounded admission pass counts the reported steps in a structurally
valid job, so a newly claimed over-limit job remains durably claimed and still returns
authenticated `204 No Content`.

Every structurally valid newly claimed completed job updates the unlabeled histogram
`github_workflow_job_steps` once with its reported step count, regardless of later acceptance or
rejection. The histogram buckets are `0`, `5`, `10`, `20`, `40`, `64`, `128`, `256`, `512`, and
`1024` steps, plus Prometheus `+Inf`. Accepted jobs at or below the inclusive limit emit every
reported step. Over-limit jobs emit no partial trace and increment
`github_workflow_job_trace_rejections_total{reason="too_many_steps"}` once.

Each accepted projection creates an independent root named `github.workflow.job`, with a new trace
identity unrelated to the live `http.request` trace. Every projected step is a direct child named
`github.workflow.step`; payload-provided names never become span names. The service does not create
a workflow-run root, persist workflow data, correlate jobs across deliveries, or mutate merge-queue
state for a workflow-job event.

A job uses its exact RFC 3339 `started_at` and `completed_at` values only when both parse and start
is not after completion; this is marked `timing_source=reported`. Otherwise it is instantaneous at a
valid completion time, or at the request receipt time when completion is missing or malformed, and
is marked `timing_source=fallback`. A step uses reported timing only when both values parse, are
ordered, and lie inside the selected job interval. Every other step interval is instantaneous at the
selected job end and marked as fallback.

Conclusions are normalized to `success`, `failure`, `cancelled`, `skipped`, `timed_out`, `neutral`,
or `other`. The CI/CD result is respectively `success`, `failure`, `cancellation`, `skip`, or
`timeout` where that semantic value exists; it is omitted for `neutral` and `other`. `success` sets
OpenTelemetry status OK, `failure` and `timed_out` set error status with a fixed description, and
all other conclusions leave status unset. Raw unknown conclusions are discarded.

The workflow root may contain only these validated span-only identifiers: canonical repository
name, delivery UUID, workflow run ID, positive run attempt, workflow job ID, valid full head SHA,
and at most the first 20 positive pull-request numbers. The run ID is exported under both
`cicd.pipeline.run.id` and `github.workflow.run.id`. The validated job ID is exported as a decimal
string under both `github.workflow.job.id` and the root's `cicd.pipeline.task.run.id`; each step's
`cicd.pipeline.task.run.id` is derived as `<job-id>:<positive-step-number>`. Workflow, job, and step
display names are span-only. Sanitization removes all Unicode control characters, retains at most
the first 128 remaining Unicode scalar values, and omits an empty result.

Commands, output, logs, actors or other user data, raw or derived URLs, request bodies and
arbitrary payload fragments, secrets, signatures, authorization and other headers, unsupported
actions, and raw unknown conclusions are not represented in workflow telemetry. They do not enter
OTLP traces, OTLP logs, structured stderr, or Prometheus exposition. Approved identifiers and
sanitized names enter spans only, except for one parentless structured warning on a newly claimed
`too_many_steps` rejection. That warning may contain only `repository_name`, `delivery_id`,
`workflow_run_id`, `workflow_run_attempt`, `workflow_job_id`, `step_count`, and `step_limit`.
Operators can query `GET /repos/{owner}/{repo}/actions/jobs/{job_id}` with the canonical
repository name and `workflow_job_id`, and can use the delivery UUID to correlate GitHub webhook
delivery records.

Historical spans use the existing bounded non-blocking trace queue. The request path does not wait
for export or retry collector failures synchronously. Queue drops and export failures increment the
bounded telemetry diagnostic counters described above but never change the authenticated
`204 No Content` response, readiness, generic webhook metrics, or merge-queue state. Collector
endpoints and exporter error details are not exposed in responses or application logs.

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
5. Record that telemetry provider shutdown is starting, close both telemetry admission boundaries,
   and begin trace and log provider shutdown concurrently.
6. Wait at most the separate shared `GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS` deadline for both providers.
7. Exit normally after a successful HTTP/retention drain even when telemetry shutdown fails or
   times out; emit only normalized direct diagnostics and bounded Prometheus accounting.
8. Preserve a pre-existing startup or server error after telemetry cleanup instead of replacing it
   with a provider error.

The timeout defaults to 30 seconds and must be a positive integer. For example, to request a
five-second drain:

```bash
GHE_SHUTDOWN_TIMEOUT_SECONDS=5 github_webhook_exporter
```

Service managers should send SIGTERM and configure their own termination grace period to exceed
the sum of `GHE_SHUTDOWN_TIMEOUT_SECONDS` and `GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS`, plus a small
process-exit allowance. SIGINT is primarily useful for an interactive local run. Startup failures
after telemetry initialization and server errors use the same final provider cleanup path. Repeated
runtime shutdown calls are idempotent and never invoke either provider twice.

Lifecycle logs contain the bound address, normalized signal kind, completion, and timeout duration.
They do not log runtime configuration, authorization headers, request bodies, encryption material,
or repository secrets.
