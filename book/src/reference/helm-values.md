# Helm values

The [chart README](https://github.com/PeterGrace/github_webhook_exporter/blob/main/charts/github-webhook-exporter/README.md)
is the authoritative reference for every value in `values.yaml`, with defaults and constraints.
This table is only a map of the value groups, so you know where to look.

| Value group | Covers |
| --- | --- |
| `replicaCount`, `maintenanceMode` | Fixed singleton; `maintenanceMode` renders zero replicas for a stopped upgrade or restore window. |
| `image.*` | Repository, tag, pull policy. An empty tag selects the chart `appVersion`. |
| `existingSecret.*` | Name of your Secret and the key names projected as `GHE_MASTER_KEY`, `GHE_ADMIN_TOKEN`, optional OTLP header keys, and the optional GitHub App private key. |
| `service.*` | Fixed `ClusterIP`; `service.port` drives the Service, container port, probes, and the application listener together. |
| `application.*` | Shutdown deadline, webhook body limit, workflow-job step cap, required-check cache lifetime, `RUST_LOG`. |
| `githubApp.*` | Optional GitHub App identifiers and API base URL enabling branch-protection required-check lookups. |
| `retention.*` | Delivery and merge-queue retention windows and the prune interval. |
| `telemetry.*` | OTLP endpoints, timeouts, service name, queue capacity, batch size, shutdown deadline. |
| `persistence.*` | Storage class, fixed `ReadWriteOnce` access mode, PVC size. |
| `resources.*`, `probes.*`, `terminationGracePeriodSeconds` | Requests and limits, liveness and readiness tuning, pod grace period. |
| `podDisruptionBudget.*` | Optional fixed `minAvailable: 0` PDB. |
| `webhookIngress.*` | Optional fixed-path Ingress for webhook delivery. |
| `metrics.*` | Optional dedicated metrics Service and Prometheus Operator `ServiceMonitor`. |
| `administration.*` | Optional separate Service and Ingress for the admin API. |
| `networkPolicy.*` | Optional default-deny with selector-bounded ingress and DNS, OTLP, and GitHub egress rules. |

There is deliberately no generic `extraEnv` map. Non-secret configuration is typed and projected
through a generated ConfigMap — see [Environment variables](environment-variables.md) for what
each one controls.

The GitHub App private key is the one credential the chart projects as a **file** rather than an
environment variable. Kubernetes writes a Secret value into a mounted file verbatim, so the Secret
holds the PEM exactly as GitHub issued it; the `GHE_GITHUB_APP_PRIVATE_KEY` environment form
expects base64 and would force operators to double-encode. The chart mounts the key read-only and
sets `GHE_GITHUB_APP_PRIVATE_KEY_PATH` for you, so the key never enters the pod's environment.

`githubApp.appId`, `githubApp.installationId`, and the private-key entry in `existingSecret.keys`
are all-or-nothing: setting only some of them fails at render time rather than starting a pod that
reports every job's required status as unknown. When `networkPolicy.enabled` is also set, add a
`networkPolicy.egress.github` rule — NetworkPolicy matches addresses rather than hostnames, so its
peers are normally an `ipBlock` for an egress gateway rather than a pod selector.
