# GitHub Webhook Exporter Helm Chart

This chart installs the core GitHub webhook exporter workload as one Kubernetes StatefulSet with a
persistent SQLite volume. It renders one ClusterIP Service, one ConfigMap, one PVC template, and an
optional PodDisruptionBudget. It does not render a Secret, Ingress, NetworkPolicy, ServiceMonitor,
or backup and restore resources.

## Prerequisites

- Helm 3 or newer and access to a Kubernetes cluster.
- `kubectl` for creating the required existing Secret.
- A persistent-volume implementation that supports `ReadWriteOnce` and can make the mount writable
  by UID/GID 65532.
- Kubernetes `policy/v1` support when the optional PodDisruptionBudget is enabled.
- Access to a compatible `linux/amd64` application image.

The chart defaults to `ghcr.io/petergrace/github-webhook-exporter`. When `image.tag` is empty, the
StatefulSet uses the chart `appVersion`, currently `0.1.0`. Issue #50 supplies publication of the
required GHCR release. Until that image exists, set `image.repository` and `image.tag` to an image
that your cluster can pull.

## Install

The chart requires one existing Secret in the release namespace. The default Secret name and key
names are `github-webhook-exporter`, `master-key`, and `admin-token`. `GHE_MASTER_KEY` must be the
base64 encoding of exactly 32 random bytes, and `GHE_ADMIN_TOKEN` must be non-empty. Read both
required values from the operator's shell environment; do not commit them to a values file or
manifest.

```bash
: "${GHE_MASTER_KEY:?set GHE_MASTER_KEY in the operator shell}"
: "${GHE_ADMIN_TOKEN:?set GHE_ADMIN_TOKEN in the operator shell}"

kubectl create namespace github-webhook-exporter
(
  set -Eeuo pipefail
  umask 077
  secret_directory="$(mktemp -d)"
  cleanup_secret_files() {
    rm -rf -- "${secret_directory}"
  }
  trap cleanup_secret_files EXIT
  trap 'exit 1' HUP INT TERM

  printf '%s' "${GHE_MASTER_KEY}" >"${secret_directory}/master-key"
  printf '%s' "${GHE_ADMIN_TOKEN}" >"${secret_directory}/admin-token"
  unset GHE_MASTER_KEY GHE_ADMIN_TOKEN
  kubectl --namespace github-webhook-exporter create secret generic github-webhook-exporter \
    --from-file="master-key=${secret_directory}/master-key" \
    --from-file="admin-token=${secret_directory}/admin-token"
)

helm upgrade --install github-webhook-exporter charts/github-webhook-exporter \
  --namespace github-webhook-exporter
```

If the required GHCR release is not yet available, select an already-published image without
placing credentials in Helm values:

```bash
: "${GHE_IMAGE_REPOSITORY:?set GHE_IMAGE_REPOSITORY in the operator shell}"
: "${GHE_IMAGE_TAG:?set GHE_IMAGE_TAG in the operator shell}"

helm upgrade --install github-webhook-exporter charts/github-webhook-exporter \
  --namespace github-webhook-exporter \
  --set-string image.repository="${GHE_IMAGE_REPOSITORY}" \
  --set-string image.tag="${GHE_IMAGE_TAG}"
```

Set `existingSecret.name` or its key-name values when the existing Secret uses different metadata.
Setting an optional OTLP header key name projects that key from the same Secret; an empty key name
omits the corresponding environment variable. The chart never reads or copies Secret data into its
ConfigMap.

## Values

| Value | Default | Description |
| --- | --- | --- |
| `replicaCount` | `1` | Fixed singleton count; every other value is rejected. |
| `image.repository` | `ghcr.io/petergrace/github-webhook-exporter` | Container repository. |
| `image.tag` | `""` | Container tag; empty selects `Chart.appVersion`. |
| `image.pullPolicy` | `IfNotPresent` | Kubernetes image pull policy. |
| `existingSecret.name` | `github-webhook-exporter` | Existing Secret containing every configured secret key. |
| `existingSecret.keys.masterKey` | `master-key` | Required key projected as `GHE_MASTER_KEY`. |
| `existingSecret.keys.adminToken` | `admin-token` | Required key projected as `GHE_ADMIN_TOKEN`. |
| `existingSecret.keys.otlpHeaders` | `""` | Optional key projected as generic OTLP headers; empty omits it. |
| `existingSecret.keys.otlpTracesHeaders` | `""` | Optional key projected as trace OTLP headers; empty omits it. |
| `existingSecret.keys.otlpLogsHeaders` | `""` | Optional key projected as log OTLP headers; empty omits it. |
| `service.type` | `ClusterIP` | Fixed supported Service type. |
| `service.port` | `8080` | Service, container, probe, and `[::]` application listener port. |
| `application.shutdownTimeoutSeconds` | `30` | Drain deadline; range `1..=300` seconds. |
| `application.webhookBodyLimitBytes` | `2097152` | Maximum webhook request body size. |
| `application.workflowJobMaxSteps` | `256` | Maximum admitted steps in a completed workflow job. |
| `application.rustLog` | `info` | Rust logging filter. |
| `retention.deliveryDays` | `7` | Authenticated delivery-claim retention in days. |
| `retention.mergeQueueDays` | `90` | Completed merge-queue-attempt retention in days. |
| `retention.pruneIntervalSeconds` | `3600` | Shared retention pruning interval in seconds. |
| `telemetry.endpoint` | `""` | Generic OTLP/HTTP endpoint; empty disables the generic endpoint. |
| `telemetry.tracesEndpoint` | `""` | Trace-specific OTLP/HTTP endpoint; empty omits the override. |
| `telemetry.logsEndpoint` | `""` | Log-specific OTLP/HTTP endpoint; empty omits the override. |
| `telemetry.timeoutMilliseconds` | `10000` | Generic OTLP export timeout in milliseconds. |
| `telemetry.tracesTimeoutMilliseconds` | `null` | Trace timeout override; null inherits the generic timeout. |
| `telemetry.logsTimeoutMilliseconds` | `null` | Log timeout override; null inherits the generic timeout. |
| `telemetry.serviceName` | `github-webhook-exporter` | OpenTelemetry service name. |
| `telemetry.queueCapacity` | `2048` | Bounded queue capacity per enabled signal. |
| `telemetry.batchSize` | `512` | Maximum export batch; cannot exceed queue capacity. |
| `telemetry.shutdownTimeoutSeconds` | `5` | Telemetry deadline; range `1..=120` seconds. |
| `persistence.storageClass` | `null` | Storage class selection; see [Storage](#storage). |
| `persistence.accessModes` | `[ReadWriteOnce]` | Fixed one-element PVC access-mode list. |
| `persistence.size` | `1Gi` | Requested PVC storage at installation. |
| `resources.requests.cpu` | `50m` | Container CPU request. |
| `resources.requests.memory` | `64Mi` | Container memory request. |
| `resources.limits.cpu` | `500m` | Container CPU limit. |
| `resources.limits.memory` | `256Mi` | Container memory limit. |
| `probes.liveness.initialDelaySeconds` | `5` | Liveness initial delay; range `1..=300`. |
| `probes.liveness.periodSeconds` | `10` | Liveness period; range `1..=300`. |
| `probes.liveness.timeoutSeconds` | `2` | Liveness request timeout; range `1..=60`. |
| `probes.liveness.failureThreshold` | `3` | Liveness failure threshold; range `1..=10`. |
| `probes.readiness.initialDelaySeconds` | `2` | Readiness initial delay; range `1..=300`. |
| `probes.readiness.periodSeconds` | `5` | Readiness period; range `1..=300`. |
| `probes.readiness.timeoutSeconds` | `2` | Readiness request timeout; range `1..=60`. |
| `probes.readiness.failureThreshold` | `3` | Readiness failure threshold; range `1..=10`. |
| `terminationGracePeriodSeconds` | `40` | Pod grace; range `1..=600` seconds and strict sum applies. |
| `podDisruptionBudget.enabled` | `false` | Render the fixed `minAvailable: 0` PDB. |

The schema intentionally exposes no generic `extraEnv` map. Typed, non-secret application,
retention, and telemetry values are projected through the generated ConfigMap. The fixed database
path, downward-API pod metadata, and existing-Secret references are rendered directly in the
StatefulSet. `service.port` is the single HTTP port setting: the Service, container, named probes,
and IPv6 wildcard application listener all derive from it.

## Storage

The application is one SQLite writer, so `replicaCount` is fixed at `1` and
`persistence.accessModes` is fixed at `[ReadWriteOnce]`. `ReadWriteOnce` is an attachment contract,
not proof that two processes can never access a volume; do not treat it as multi-writer protection.
The chart does not support ephemeral data storage or multiple replicas.

`persistence.storageClass: null` omits `storageClassName` and allows cluster-default selection.
Setting `persistence.storageClass` to the empty string renders `storageClassName: ""`, which requests
no storage class and disables default dynamic class selection. A non-empty string selects that
class.

The pod and container run as UID/GID 65532, and the pod sets `fsGroup: 65532` with
`fsGroupChangePolicy: OnRootMismatch`. A CSI driver or other provider that ignores `fsGroup` must
provision the mounted directory with ownership and permissions writable by UID/GID 65532. The root
filesystem is read-only; the PVC mounted at `/var/lib/github-webhook-exporter` is the application's
writable data location.

Treat storage class, access mode, and size as installation-time claim settings. Kubernetes may
reject updates to a StatefulSet volume claim template, and the chart does not perform claim resize,
data migration, backup, or restore. Claims created from the StatefulSet template are normally
retained when the release is removed; inspect and preserve the PVC before deleting data explicitly.

## Probes and telemetry

The liveness probe calls `GET /health/live` and tests whether the HTTP process can serve requests;
it does not query SQLite or any external system. The readiness probe calls `GET /health/ready` and
runs the application's lightweight SQLite check. A readiness failure removes the pod from Service
endpoints without making liveness fail.

OTLP collector availability and export failures affect neither probe. Remote telemetry is optional:
with no generic or signal-specific endpoint configured, the service retains local structured
logging without creating a remote provider.

## Security and secrets

The pod runs as non-root UID/GID 65532, uses the runtime-default seccomp profile, and disables the
default ServiceAccount token mount. The container forbids privilege escalation, drops every Linux
capability, and uses a read-only root filesystem. These controls do not replace storage-provider
permission setup or network access controls; this chart does not supply Ingress or NetworkPolicy.

Non-secret typed configuration enters the pod from the generated ConfigMap. Required credentials
and optional OTLP headers enter only through references to the configured existing Secret. The
required key references are not optional, and the chart never creates a Secret or renders Secret
values.

## Shutdown, disruption, and upgrades

The chart enforces this strict relationship at render time:

```text
terminationGracePeriodSeconds
  > application.shutdownTimeoutSeconds + telemetry.shutdownTimeoutSeconds
```

The defaults are `40 > 30 + 5`. The maximum values also permit a valid combination because
`600 > 300 + 120`. This leaves a positive Kubernetes process-exit margin after the application
drain and telemetry shutdown boundaries. The pod defines no `preStop` delay; SIGTERM starts
application shutdown directly. The validation checks configuration arithmetic, not runtime
lifecycle behavior.

The optional PodDisruptionBudget is disabled by default. When enabled, it has fixed
`minAvailable: 0` semantics. It cannot keep a singleton available, does not prevent voluntary
disruption, and provides no protection from involuntary failures.

The StatefulSet pod template contains a deterministic checksum of the rendered ConfigMap. Changing
any ConfigMap-backed value during `helm upgrade` changes that annotation and triggers the configured
`RollingUpdate`, so the replacement pod reads the new environment values. The strategy and
`ReadWriteOnce` do not guarantee safe volume handoff on every storage provider. The
Recreate-equivalent operator procedure is deferred to #48; until that procedure is available, do
not claim that chart-driven upgrades prevent overlapping attachment or SQLite writers on providers
where overlap is possible.

## Validation

From the repository root, run:

```bash
just helm-lint
just helm-test
just helm-kind-acceptance
```

`helm-lint` validates chart metadata and defaults. `helm-test` checks schema, rendering, security,
storage, Secret-reference, probe, PDB, and shutdown-boundary contracts. `helm-kind-acceptance`
creates a disposable Kind cluster and confirms that Kubernetes accepts the StatefulSet, Service,
ConfigMap, and PVC APIs. It does not wait for the unpublished application image, test readiness, or
exercise runtime lifecycle behavior.
