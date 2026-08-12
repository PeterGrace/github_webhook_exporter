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
  ghcr.io/petergrace/github-webhook-exporter:0.1.5
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

For a hands-on walkthrough that delivers a real signed webhook and watches it become a metric, see
[Get a webhook flowing into metrics](https://petergrace.github.io/github_webhook_exporter/tutorials/getting-started.html)
on the [documentation site](https://petergrace.github.io/github_webhook_exporter/).

## Deploy on Kubernetes

The supported deployment is the Helm chart, which installs one StatefulSet replica with a
`ReadWriteOnce` PVC for SQLite, a ClusterIP Service, and non-root UID/GID 65532.

Create the Secret the chart expects, then install:

```bash
kubectl create secret generic github-webhook-exporter \
  --from-literal=master-key="$(openssl rand -base64 32)" \
  --from-literal=admin-token="$(openssl rand -hex 32)"

helm install github-webhook-exporter \
  oci://ghcr.io/petergrace/charts/github-webhook-exporter --version 0.1.5
```

The chart never creates a Secret; it only references one you supply.

**[The chart README](charts/github-webhook-exporter/README.md) is the authoritative deployment
reference.** It documents every value in `values.yaml` with defaults and constraints, and covers
storage, probes, ingress, network policy, backup and restore, and upgrade behavior. See
[How to deploy with Helm](https://petergrace.github.io/github_webhook_exporter/how-to/deploy-with-helm.html)
for a full walkthrough and
[Helm values](https://petergrace.github.io/github_webhook_exporter/reference/helm-values.html) for
a map of the value groups.

There is deliberately no generic `extraEnv` map. Non-secret configuration is typed and projected
through a generated ConfigMap.

## Configuration, HTTP API, and metrics

The chart sets everything below for you; it matters when running the container or binary
directly. Full tables live on the documentation site so they're maintained in one place:

- [Environment variables](https://petergrace.github.io/github_webhook_exporter/reference/environment-variables.html) —
  required (`GHE_DATABASE_PATH`, `GHE_MASTER_KEY`, `GHE_ADMIN_TOKEN`), optional application
  settings, and OpenTelemetry export configuration.
- [HTTP API](https://petergrace.github.io/github_webhook_exporter/reference/http-api.html) —
  health endpoints, the webhook endpoint's headers and status codes, and the admin API.
- [Metrics](https://petergrace.github.io/github_webhook_exporter/reference/metrics.html) — every
  Prometheus series and its labels; all series are prefixed `github_` and carry bounded,
  closed-vocabulary label sets so cardinality stays fixed no matter how busy the organization is.

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
a pull request and publishes the tag. See
[How to release a new version](https://petergrace.github.io/github_webhook_exporter/how-to/release-a-new-version.html) —
direct pushes to `main` are rejected by a repository ruleset, so the tag is published separately
from the release commit.

## Documentation

**[The documentation site](https://petergrace.github.io/github_webhook_exporter/)** is the
canonical home for tutorials, how-to guides, reference material, and design rationale, built from
[`book/src`](book/src). A few things live outside it:

| Document | Contents |
| --- | --- |
| [Chart README](charts/github-webhook-exporter/README.md) | Every Helm value, storage, probes, ingress, network policy, backup and restore. |
| `changelog/` | Dated notes for each change. |

`docs/superpowers/` holds historical plans and design specs, and `docs/build-spec.md` is a
superseded early draft. Both record what was true when written and are not maintained as current
documentation.
