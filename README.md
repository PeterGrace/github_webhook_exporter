# GitHub Webhook Exporter

A single-instance Rust service that turns GitHub webhook deliveries into Prometheus metrics and
OpenTelemetry traces, without persisting payloads.

Each configured repository authenticates with its own webhook secret. Deliveries are verified by
`X-Hub-Signature-256` HMAC over the exact request bytes, counted into bounded-cardinality metrics,
and then discarded. Repository names, pull-request numbers, commit SHAs, and delivery IDs are never
used as metric labels, so cardinality stays fixed no matter how busy the organization is.

Beyond plain event counting it also:

- tracks merge-queue attempts per pull request and records their outcome and duration;
- projects completed `workflow_job` events into historical OTLP traces, with a
  `github.workflow.job` root span and one `github.workflow.step` child per step;
- deduplicates GitHub redeliveries by delivery ID, so retries do not double-count.

SQLite holds only repository configuration, delivery IDs for deduplication, and merge-queue attempt
state. A background task prunes the last two on an interval.

## Quick start

The service needs three secrets and a writable database path. Generate a master key first:

```bash
openssl rand -base64 32
```

Run the published image:

```bash
docker run --rm -p 8080:8080 \
  -v github-webhook-exporter:/var/lib/github-webhook-exporter \
  -e GHE_DATABASE_PATH=/var/lib/github-webhook-exporter/github-webhook-exporter.db \
  -e GHE_MASTER_KEY="$(openssl rand -base64 32)" \
  -e GHE_ADMIN_TOKEN="$(openssl rand -hex 32)" \
  ghcr.io/petergrace/github-webhook-exporter:0.1.1
```

Register a repository and its webhook secret through the admin API:

```bash
curl -X POST http://localhost:8080/api/v1/repositories \
  -H "Authorization: Bearer ${GHE_ADMIN_TOKEN}" \
  -H 'Content-Type: application/json' \
  -d '{"full_name":"octocat/hello-world","webhook_secret":"the-secret-you-set-on-github"}'
```

Point the repository's GitHub webhook at `POST /webhooks/github` with content type
`application/json` and the same secret, then scrape `GET /metrics`.

## Deploy on Kubernetes

The supported deployment is the Helm chart, which installs one StatefulSet replica with a
`ReadWriteOnce` PVC for SQLite, a ClusterIP Service, and non-root UID/GID 65532.

Create the Secret the chart expects, then install:

```bash
kubectl create secret generic github-webhook-exporter \
  --from-literal=master-key="$(openssl rand -base64 32)" \
  --from-literal=admin-token="$(openssl rand -hex 32)"

helm install github-webhook-exporter \
  oci://ghcr.io/petergrace/charts/github-webhook-exporter --version 0.1.1
```

The chart never creates a Secret; it only references one you supply.

**[The chart README](charts/github-webhook-exporter/README.md) is the authoritative deployment
reference.** It documents every value in `values.yaml` with defaults and constraints, and covers
storage, probes, ingress, network policy, backup and restore, and upgrade behavior. The table below
is only a map of the value groups so you know where to look.

| Value group | Covers |
| --- | --- |
| `replicaCount`, `maintenanceMode` | Fixed singleton; `maintenanceMode` renders zero replicas for a stopped upgrade or restore window. |
| `image.*` | Repository, tag, pull policy. An empty tag selects the chart `appVersion`. |
| `existingSecret.*` | Name of your Secret and the key names projected as `GHE_MASTER_KEY`, `GHE_ADMIN_TOKEN`, and optional OTLP headers. |
| `service.*` | Fixed `ClusterIP`; `service.port` drives the Service, container port, probes, and the application listener together. |
| `application.*` | Shutdown deadline, webhook body limit, workflow-job step cap, `RUST_LOG`. |
| `retention.*` | Delivery and merge-queue retention windows and the prune interval. |
| `telemetry.*` | OTLP endpoints, timeouts, service name, queue capacity, batch size, shutdown deadline. |
| `persistence.*` | Storage class, fixed `ReadWriteOnce` access mode, PVC size. |
| `resources.*`, `probes.*`, `terminationGracePeriodSeconds` | Requests and limits, liveness and readiness tuning, pod grace period. |
| `podDisruptionBudget.*` | Optional fixed `minAvailable: 0` PDB. |
| `webhookIngress.*` | Optional fixed-path Ingress for webhook delivery. |
| `metrics.*` | Optional dedicated metrics Service and Prometheus Operator ServiceMonitor. |
| `administration.*` | Optional separate Service and Ingress for the admin API. |
| `networkPolicy.*` | Optional default-deny with selector-bounded ingress and DNS/OTLP egress rules. |

There is deliberately no generic `extraEnv` map. Non-secret configuration is typed and projected
through a generated ConfigMap.

## Configuration

The chart sets these for you; they matter when running the container or binary directly.

Required, with no default. Startup fails if any is missing or invalid:

| Variable | Contract |
| --- | --- |
| `GHE_DATABASE_PATH` | Writable SQLite file path. |
| `GHE_MASTER_KEY` | Base64 of exactly 32 random bytes. Derives the repository-secret encryption key. |
| `GHE_ADMIN_TOKEN` | Non-empty bearer token for the admin API. |

Optional application settings:

| Variable | Default | Contract |
| --- | --- | --- |
| `GHE_BIND_ADDRESS` | `[::]:8080` | Listener address. |
| `GHE_SHUTDOWN_TIMEOUT_SECONDS` | `30` | Drain deadline. |
| `GHE_WEBHOOK_BODY_LIMIT_BYTES` | `2097152` | Maximum webhook body. This default is also the enforced maximum. |
| `GHE_WORKFLOW_JOB_MAX_STEPS` | `256` | Step cap for workflow-job traces; range `1..=1024`. |
| `GHE_DELIVERY_RETENTION_DAYS` | `7` | Delivery-ID retention. |
| `GHE_MERGE_QUEUE_RETENTION_DAYS` | `90` | Completed merge-queue attempt retention. |
| `GHE_DELIVERY_PRUNE_INTERVAL_SECONDS` | `3600` | Retention sweep interval. |
| `GHE_OTEL_QUEUE_CAPACITY` | `2048` | Bounded queue per enabled signal. |
| `GHE_OTEL_BATCH_SIZE` | `512` | Export batch size; cannot exceed queue capacity. |
| `GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS` | `5` | Telemetry flush deadline. |
| `RUST_LOG` | `info` | `tracing_subscriber` filter directive. |

OpenTelemetry export is entirely off unless at least one endpoint variable is set:

| Variable | Default | Contract |
| --- | --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | unset | Base OTLP/HTTP endpoint. `v1/traces` and `v1/logs` are appended. |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | derived | Complete trace endpoint, including `/v1/traces`. |
| `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | derived | Complete log endpoint, including `/v1/logs`. |
| `OTEL_EXPORTER_OTLP_HEADERS` | unset | Headers for both signals. |
| `OTEL_EXPORTER_OTLP_TRACES_HEADERS` | inherits generic | An explicitly empty value clears inherited headers. |
| `OTEL_EXPORTER_OTLP_LOGS_HEADERS` | inherits generic | An explicitly empty value clears inherited headers. |
| `OTEL_EXPORTER_OTLP_TIMEOUT` | `10000` | Export timeout in milliseconds. |
| `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT` | inherits generic | Trace export timeout in milliseconds. |
| `OTEL_EXPORTER_OTLP_LOGS_TIMEOUT` | inherits generic | Log export timeout in milliseconds. |
| `OTEL_SERVICE_NAME` | `github-webhook-exporter` | Reported service name. |
| `OTEL_RESOURCE_ATTRIBUTES` | unset | Comma-separated `key=value`. Only `k8s.pod.name` and `k8s.namespace.name` are retained; other keys are dropped. Malformed entries are fatal. |

Structured logging to stderr is always on, independent of OTLP.

## HTTP endpoints

Unauthenticated:

| Method and path | Purpose |
| --- | --- |
| `GET /health/live` | Process liveness. Never touches SQLite. |
| `GET /health/ready` | Readiness, including a SQLite check. |
| `GET /metrics` | OpenMetrics exposition for Prometheus. |
| `POST /webhooks/github` | Webhook ingestion. Requires `X-GitHub-Event`, `X-GitHub-Delivery`, and `X-Hub-Signature-256`. |

`POST /webhooks/github` answers `204` when accepted, including for duplicate deliveries; `400`
malformed; `401` unknown repository or bad signature; `413` too large; `415` wrong content type;
`503` database unavailable. Bodies are never echoed back.

The admin API requires `Authorization: Bearer <GHE_ADMIN_TOKEN>` and never returns webhook secrets:

| Method and path | Purpose |
| --- | --- |
| `POST /api/v1/repositories` | Register a repository and its webhook secret. |
| `GET /api/v1/repositories` | List configured repositories. |
| `GET /api/v1/repositories/{id}` | Fetch repository metadata. |
| `PATCH /api/v1/repositories/{id}` | Rename, enable, disable, or rotate the secret. |
| `DELETE /api/v1/repositories/{id}` | Remove the configuration. |

## Metrics

All series are prefixed `github_` and carry bounded label sets.

| Metric | Labels |
| --- | --- |
| `github_webhook_requests_total` | `result` |
| `github_webhook_events_total` | `event_type`, `action` |
| `github_webhook_processing_duration_seconds` | `result` |
| `github_webhook_request_body_bytes` | none |
| `github_webhook_duplicates_total` | none |
| `github_webhook_processing_failures_total` | `stage` |
| `github_repository_configurations` | none |
| `github_merge_group_events_total` | `action`, `reason` |
| `github_merge_queue_pr_outcomes_total` | `outcome`, `reason` |
| `github_merge_queue_attempt_duration_seconds` | `outcome` |
| `github_merge_queue_transition_failures_total` | `reason` |
| `github_workflow_job_steps` | none |
| `github_workflow_job_trace_rejections_total` | `reason` |
| `github_telemetry_export_failures_total` | `signal`, `reason` |
| `github_telemetry_dropped_records_total` | `signal`, `reason` |

Unrecognized webhook event types collapse to `event_type="other"` and unrecognized actions to
`action="other"`, which is what keeps cardinality fixed.

## Development

```bash
cargo build
just test                            # cargo test --all-targets
just fmt                             # cargo fmt --all -- --check
cargo clippy --all-targets -- -D warnings
```

Chart and container checks:

```bash
just helm-static                     # lint, render, kubeconform, policy, secrets, package
just image-build                     # linux/amd64 production image
just image-smoke                     # image contract checks
just helm-kind-lifecycle             # disposable Kind cluster lifecycle
```

Set `CONTAINER_IMAGE` to build or smoke-test a registry-specific tag.

## Releasing

`just release-patch` prepares the version bump and tag locally; `just release-ship` lands it through
a pull request and publishes the tag. See [RELEASE.md](RELEASE.md) — direct pushes to `main` are
rejected by a repository ruleset, so the tag is published separately from the release commit.

## Documentation

| Document | Contents |
| --- | --- |
| [Chart README](charts/github-webhook-exporter/README.md) | Every Helm value, storage, probes, ingress, network policy, backup and restore. |
| [Operations guide](docs/operations.md) | Startup behavior, container contracts, GHCR release semantics, telemetry, disruption and upgrades. |
| [RELEASE.md](RELEASE.md) | How to cut and publish a release. |
| `changelog/` | Dated notes for each change. |

`docs/superpowers/` holds historical plans and design specs. They record what was true when written
and are not maintained as current documentation.
