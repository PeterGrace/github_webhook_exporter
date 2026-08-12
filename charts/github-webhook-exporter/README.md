# GitHub Webhook Exporter Helm Chart

This chart installs the core GitHub webhook exporter workload as one Kubernetes StatefulSet with a
persistent SQLite volume. It renders one ClusterIP Service, one ConfigMap, one PVC template, and
optional exposure, NetworkPolicy, ServiceMonitor, and PodDisruptionBudget resources. It does not
render a Secret or backup and restore resources. Every exposure and network-boundary resource is
disabled by default.

## Prerequisites

- Helm 3 or newer and access to a Kubernetes cluster.
- `kubectl` for creating the required existing Secret.
- A persistent-volume implementation that supports `ReadWriteOnce` and can make the mount writable
  by UID/GID 65532.
- Kubernetes `policy/v1` support when the optional PodDisruptionBudget is enabled.
- Access to a compatible `linux/amd64` application image.

The chart defaults to `ghcr.io/petergrace/github-webhook-exporter`. When `image.tag` is empty, the
StatefulSet uses the chart `appVersion`, currently `0.1.4`.

## Release consumption

- Stable `vMAJOR.MINOR.PATCH` tags publish one immutable image and one immutable Helm OCI chart.
- Pull requests and `main` remain validation-only; only a stable tag publishes the matching `ghcr.io/petergrace/github-webhook-exporter:MAJOR.MINOR.PATCH` image.
- The chart lives at `oci://ghcr.io/petergrace/charts/github-webhook-exporter`.
- Consume a release with matching versions on both sides:

```bash
docker pull ghcr.io/petergrace/github-webhook-exporter:0.1.4
helm pull oci://ghcr.io/petergrace/charts/github-webhook-exporter --version 0.1.4
helm install github-webhook-exporter oci://ghcr.io/petergrace/charts/github-webhook-exporter --version 0.1.4
```

- Published version tags are immutable. The workflow never publishes `latest`, branch, SHA, or prerelease tags.
- The client-side overwrite guard is not atomic with the registry push, so repository administrators must also prevent concurrent or manual pushes to release tags.
- If validation fails, rerun the original failed workflow attempt without moving the tag.
- Only the image-existing/chart-missing state with an exact matching digest may resume as chart-only recovery.
- Completed, chart-only, and digest-conflict states fail closed without overwrite.

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
| `replicaCount` | `1` | Fixed running singleton count; every other value is rejected. |
| `maintenanceMode` | `false` | Render zero replicas for an explicit stopped upgrade or restore window. |
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
| `webhookIngress.enabled` | `false` | Render the fixed-path webhook Ingress. |
| `webhookIngress.className` | `""` | Optional webhook ingress class. |
| `webhookIngress.annotations` | `{}` | Non-secret webhook Ingress annotations. |
| `webhookIngress.host` | `""` | Optional webhook hostname; empty renders a hostless rule. |
| `webhookIngress.tls` | `[]` | TLS Secret metadata and hosts managed by the platform. |
| `metrics.service.enabled` | `false` | Render the dedicated metrics Service. |
| `metrics.service.port` | `8080` | Dedicated metrics Service port. |
| `metrics.service.annotations` | `{}` | Non-secret metrics Service annotations. |
| `metrics.serviceMonitor.enabled` | `false` | Render a ServiceMonitor; requires the metrics Service. |
| `metrics.serviceMonitor.labels` | `{}` | Labels used by the installed Prometheus Operator selector. |
| `metrics.serviceMonitor.interval` | `30s` | Prometheus scrape interval. |
| `metrics.serviceMonitor.scrapeTimeout` | `10s` | Prometheus scrape timeout. |
| `administration.service.enabled` | `false` | Render the dedicated administrative Service. |
| `administration.service.port` | `8080` | Administrative Service port. |
| `administration.service.annotations` | `{}` | Non-secret administrative Service annotations. |
| `administration.ingress.enabled` | `false` | Render repository administration; requires its Service. |
| `administration.ingress.className` | `""` | Management-only ingress class. |
| `administration.ingress.annotations` | `{}` | Non-secret administrative Ingress annotations. |
| `administration.ingress.host` | `""` | Optional administrative hostname. |
| `administration.ingress.tls` | `[]` | Administrative TLS Secret metadata and hosts. |
| `networkPolicy.enabled` | `false` | Default deny ingress and egress for exporter pods. |
| `networkPolicy.ingress.*` | disabled | Selector-bounded controller, Prometheus, and management rules. |
| `networkPolicy.egress.dns` | disabled | Selector-bounded TCP/UDP port 53 rule. |
| `networkPolicy.egress.otlp` | disabled | Explicit selector/CIDR peers and TCP collector ports. |

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

Standard Kubernetes NetworkPolicy always permits ingress from the pod's node, so kubelet probes do
not require an explicit allowance in this chart's policy. CNI-specific host-firewall policies are
separate controls; operators using them must verify that they independently permit kubelet
liveness and readiness probes.

OTLP collector availability and export failures affect neither probe. Remote telemetry is optional:
with no generic or signal-specific endpoint configured, the service retains local structured
logging without creating a remote provider.

## Security and secrets

The pod runs as non-root UID/GID 65532, uses the runtime-default seccomp profile, and disables the
default ServiceAccount token mount. The container forbids privilege escalation, drops every Linux
capability, and uses a read-only root filesystem. These controls do not replace storage-provider
permission setup or network access controls. Optional Ingress and NetworkPolicy resources remain
disabled until an operator configures platform-specific selectors and routing metadata.

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
`RollingUpdate`, so the replacement pod reads the new environment values. With one replica,
Kubernetes normally terminates ordinal `0` before creating its replacement. Use this path only when
the storage provider reliably completes volume handoff before a replacement can mount the PVC.

For providers that may permit attachment overlap, perform a stopped upgrade:

```bash
: "${GHE_IMAGE_TAG:?set GHE_IMAGE_TAG to an immutable published version}"
kubectl --namespace github-webhook-exporter scale \
  statefulset/github-webhook-exporter --replicas=0
kubectl --namespace github-webhook-exporter wait --for=delete \
  pod/github-webhook-exporter-0 --timeout=180s
helm upgrade github-webhook-exporter charts/github-webhook-exporter \
  --namespace github-webhook-exporter --reuse-values \
  --set maintenanceMode=true --set-string image.tag="${GHE_IMAGE_TAG}" --wait
helm upgrade github-webhook-exporter charts/github-webhook-exporter \
  --namespace github-webhook-exporter --reuse-values \
  --set maintenanceMode=false --wait
```

`maintenanceMode=true` renders zero desired replicas without adding a sidecar or maintenance
resource, allowing Helm to record an upgrade while the exporter remains stopped. Confirm provider
volume release before the first Helm command, then disable maintenance mode to restore exactly one
replica. This procedure intentionally causes downtime. The singleton and optional fixed
`minAvailable: 0` PodDisruptionBudget cannot preserve availability during voluntary disruption.

## Back up and restore SQLite

Use the repository's digest-pinned maintenance command for a consistent online backup:

```bash
backup_name="backup-$(date -u +%Y%m%dT%H%M%SZ).db"
scripts/helm-sqlite-maintenance.sh backup \
  github-webhook-exporter github-webhook-exporter \
  data-github-webhook-exporter-0 "${backup_name}"
```

The command creates a hardened non-root Pod, runs SQLite's online `.backup`, validates the result,
and sets mode `0600`. Because online backup mounts the `ReadWriteOnce` PVC from a second Pod while
the exporter holds it, the command pins that Pod to the exporter's current node. The single-node
Kind test cannot validate cross-node CSI attachment; providers that forbid same-node multi-Pod
mounts require a coordinated offline platform snapshot instead. Move the backup to encrypted
storage or snapshot the PVC after completion; a copy retained only on the PVC does not survive PVC
loss. Copying the active database file alone is unsupported because committed state may remain in
its WAL.

Restore only in maintenance mode:

```bash
: "${BACKUP_NAME:?set BACKUP_NAME to the validated .db backup basename}"
helm upgrade github-webhook-exporter charts/github-webhook-exporter \
  --namespace github-webhook-exporter --reuse-values \
  --set maintenanceMode=true --wait
kubectl --namespace github-webhook-exporter wait --for=delete \
  pod/github-webhook-exporter-0 --timeout=180s
scripts/helm-sqlite-maintenance.sh restore \
  github-webhook-exporter github-webhook-exporter \
  data-github-webhook-exporter-0 "${BACKUP_NAME}"
helm upgrade github-webhook-exporter charts/github-webhook-exporter \
  --namespace github-webhook-exporter --reuse-values \
  --set maintenanceMode=false --wait
```

Restore refuses to start unless desired replicas are zero and ordinal `0` is absent. These are
point-in-time checks, so keep maintenance mode enabled and prevent any other controller or operator
from scaling the StatefulSet until the command exits. It validates the source and restored database,
enforces UID/GID `65532:65532` and mode `0600`, removes stale WAL
files, and retains the replaced database as `.pre-restore`. Before accepting recovery, verify
readiness and migrations, repository-secret decryption with a signed webhook, expected metrics,
pre-backup delivery deduplication, and pre-backup merge-queue state. Keep the source backup and
pre-restore file until those checks pass.

The chart does not automate backup scheduling or external retention. `helm rollback` does not undo
SQLite migrations, PVC-template changes, or incompatible application downgrades. See
[How to back up and restore SQLite](https://petergrace.github.io/github_webhook_exporter/how-to/back-up-and-restore.html)
for detailed recovery checks and troubleshooting.

## Exposure and network boundaries

The exporter has one HTTP listener. The core, metrics, and administrative Services all select the
same pod and target its named `http` port. Separate Services improve discovery and routing intent,
but they do not isolate application paths.

NetworkPolicy cannot distinguish HTTP paths on the shared listener. It permits or rejects traffic
by pod peer, IP block, protocol, and port; it cannot tell `/webhooks/github`, `/metrics`, and
`/api/v1/repositories` apart. Enforce path boundaries with the chart's fixed Ingress routes, a
platform authorization proxy, or equivalent external L7 policy. Do not claim that a dedicated
Service alone prevents access to another path.

TLS termination and certificate provisioning remain ingress-platform responsibilities. Likewise,
the chart does not install an ingress controller, Prometheus Operator, authorization proxy, DNS
implementation, or OTLP collector. Metadata maps must contain only non-secret values. The chart
never copies `existingSecret` data or OTLP header values into Services, Ingresses, ServiceMonitors,
or NetworkPolicies.

## Webhook ingress example

The webhook Ingress always uses `Exact` path `/webhooks/github`; neither path nor backend is
configurable. Supply only platform routing and TLS metadata:

```yaml
webhookIngress:
  enabled: true
  className: public
  host: hooks.example.test
  annotations:
    ingress.example.test/request-body-limit: "2Mi"
  tls:
    - secretName: webhook-tls
      hosts:
        - hooks.example.test
```

Route GitHub to `https://hooks.example.test/webhooks/github`. The chart does not create
`webhook-tls` or issue its certificate.

## Prometheus scraping example

Enable the dedicated Service alone for a platform scraper, or enable the ServiceMonitor when the
Prometheus Operator CRD is already installed:

```yaml
metrics:
  service:
    enabled: true
    port: 8080
    annotations: {}
  serviceMonitor:
    enabled: true
    labels:
      prometheus: platform
    interval: 30s
    scrapeTimeout: 10s
```

The ServiceMonitor selects only the dedicated metrics Service and requests fixed path `/metrics`.
The Service still forwards to the shared listener, so NetworkPolicy must bound the Prometheus peer.

## Management access example

Use a designated management ingress class and hostname. The administrative Ingress always routes
prefix `/api/v1/repositories` to the dedicated administrative Service:

```yaml
administration:
  service:
    enabled: true
    port: 8080
    annotations: {}
  ingress:
    enabled: true
    className: management
    host: exporter-admin.example.test
    annotations: {}
    tls:
      - secretName: exporter-admin-tls
        hosts:
          - exporter-admin.example.test
```

This route does not replace bearer-token authentication. Restrict the management ingress controller
with the policy selector below or enforce the route through an authorization proxy.

## DNS and OTLP egress example

Enabling NetworkPolicy with no allowances creates default-deny ingress and egress. Every enabled
inbound or DNS rule requires both namespace and pod selectors. The following permits a designated
ingress controller, cluster DNS, and one in-cluster collector; omitted Prometheus and management
rules remain denied:

```yaml
networkPolicy:
  enabled: true
  ingress:
    ingressController:
      enabled: true
      namespaceSelector:
        matchLabels:
          kubernetes.io/metadata.name: ingress-system
      podSelector:
        matchLabels:
          app.kubernetes.io/name: ingress-controller
  egress:
    dns:
      enabled: true
      namespaceSelector:
        matchLabels:
          kubernetes.io/metadata.name: kube-system
      podSelector:
        matchLabels:
          k8s-app: kube-dns
    otlp:
      enabled: true
      peers:
        - namespaceSelector:
            matchLabels:
              kubernetes.io/metadata.name: observability
          podSelector:
            matchLabels:
              app.kubernetes.io/name: otel-collector
      ports:
        - 4318
```

DNS labels vary by distribution; inspect the cluster's DNS pods before enabling the rule. For an
external collector, use an `ipBlock.cidr` peer instead of selectors. NetworkPolicy cannot allow a
DNS hostname, so externally resolved collector addresses must have stable operator-managed CIDRs.
The OTLP rule changes only network reachability; collector availability remains independent of
liveness and readiness.

## Validation

From the repository root, run:

```bash
just helm-static
just image-smoke
just helm-maintenance-unit
helm show chart dist/github-webhook-exporter-0.1.4.tgz
helm show values dist/github-webhook-exporter-0.1.4.tgz
helm template archive dist/github-webhook-exporter-0.1.4.tgz --kube-version 1.35.0 >/dev/null
just helm-kind-acceptance
KIND_ARTIFACT_DIRECTORY=dist/kind-lifecycle just helm-kind-lifecycle
```

`just helm-static` validates chart metadata, rendering, schema, policy, secret, and packaged
archive contracts across the supported Kubernetes range 1.31.0 through 1.35.0
(`>=1.31.0-0 <1.36.0-0`). `just image-smoke` builds the production image and checks the runtime
and persistence contract locally. The packaged chart archive is always
`dist/github-webhook-exporter-0.1.4.tgz`; use `helm show chart` and `helm show values` for local
inspection before distributing or reusing it. `helm template archive` with `--kube-version
1.35.0` inspects the packaged chart without talking to a cluster.

`just helm-kind-acceptance` creates a disposable Kind cluster and confirms that Kubernetes accepts
the StatefulSet, Service, ConfigMap, and PVC APIs. It does not start the exporter. Passing static
checks does not prove runtime behavior; passing static checks does not prove cluster lifecycle behavior.

`just helm-kind-lifecycle` is the runtime acceptance gate. It builds and loads the production image,
creates runtime-only credentials, installs the chart, and verifies probes, signed administration
and webhook traffic, bounded metrics, persistence across pod replacement, delivery deduplication,
pull-request queue state, merge-group transitions, unavailable-collector isolation, broken database
readiness, graceful SIGTERM, online backup, active-restore rejection, stopped restore, restored
ownership and mode, post-recovery application state, and an observed maximum of one running exporter
attached to the SQLite PVC during chart rollout and recovery. Sampling bounds observed Kubernetes
status; it cannot prove that overlap shorter than the sample interval is impossible. The generated cluster is deleted on
success and failure. Diagnostics default to
`dist/kind-lifecycle`, are scanned for generated credentials, signatures, and forbidden payload
material, and are uploaded by CI even when the test fails. `KEEP_KIND_CLUSTER=true` is an explicit
local debugging escape hatch; remove the printed cluster and private temporary directory manually.
