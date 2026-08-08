# Singleton StatefulSet Helm Chart Design

## Purpose

Issue #44 adds the supported Helm package for the production container introduced by #43. The
package deploys exactly one `github-webhook-exporter` process with durable SQLite storage, the
application's established health probes, bounded resources, non-root permissions, and enough
termination time for both application and telemetry shutdown.

The chart is intentionally limited to the core workload and its application Service. Secure
Ingress, metrics exposure, and NetworkPolicy resources belong to #45. Full static CI policy and
packaging validation belong to #46. Production image publication belongs to #50.

## Chart layout and versioning

The chart lives at `charts/github-webhook-exporter/` and contains:

- `Chart.yaml`
- `values.yaml`
- `values.schema.json`
- `README.md`
- `templates/_helpers.tpl`
- `templates/configmap.yaml`
- `templates/service.yaml`
- `templates/statefulset.yaml`
- `templates/pdb.yaml`
- `templates/NOTES.txt`

The initial chart version is `0.1.0`, and `appVersion` matches the Cargo package version, `0.1.0`.
The default image repository is `ghcr.io/petergrace/github-webhook-exporter`. An empty image tag
means the templates use `Chart.appVersion`; an explicit value overrides it. The default pull policy
is `IfNotPresent`.

The chart uses standard name, full-name, chart, selector, and common-label helpers. Selectors are
stable and contain no chart-version label so upgrades do not attempt to mutate immutable
StatefulSet or Service selectors.

## Workload and storage

The workload is an `apps/v1` StatefulSet with these fixed semantics:

- Exactly one replica.
- `RollingUpdate` update strategy.
- Ordered pod management.
- One container exposing the named application port, defaulting to TCP 8080.
- One `volumeClaimTemplates` entry mounted at `/var/lib/github-webhook-exporter`.
- `ReadWriteOnce` access by default and no support for an empty ephemeral data volume.
- `GHE_DATABASE_PATH` set to
  `/var/lib/github-webhook-exporter/github-webhook-exporter.db` by default.

The PVC values expose storage size, an optional storage class, and a constrained access-mode list.
Storage size must be a non-empty Kubernetes quantity. An explicitly empty storage class requests a
claim with `storageClassName: ""`; an omitted value leaves selection to the cluster default.
Supported access modes are restricted to `ReadWriteOnce` because the application is a singleton
SQLite writer.

The chart retains StatefulSet `RollingUpdate` behavior. Its README states that this does not by
itself guarantee safe volume handoff on every storage provider. Operators must use the
Recreate-equivalent rollout procedure that #48 will document before upgrades that could otherwise
permit overlapping attachment or writers.

## Service and probes

A headless service is not needed because the application has no peer-discovery requirement. The
StatefulSet's `serviceName` and the supported client endpoint use one ClusterIP Service selecting
the workload's stable selector labels.

The container defines HTTP probes on the named application port:

- Liveness: `GET /health/live`
- Readiness: `GET /health/ready`

Probe timing values are configurable within positive bounded ranges. Neither probe references OTLP
configuration or collector availability. Readiness therefore preserves the application's SQLite
probe semantics, while liveness remains process-only.

## Configuration model

Supported configuration is typed and grouped in `values.yaml`; the chart does not expose a generic
unvalidated `extraEnv` escape hatch in this iteration.

A generated ConfigMap contains non-secret environment values for:

- bind address and Rust logging filter;
- application shutdown and webhook body limits;
- workflow-job step admission;
- delivery and merge-queue retention;
- retention pruning interval;
- OTLP endpoints, timeouts, service name, resource attributes, queue capacity, batch size, and
  telemetry shutdown timeout.

Optional endpoint and resource-attribute entries are omitted when disabled rather than rendered as
misleading placeholders. Kubernetes pod name and namespace resource attributes use downward-API
environment variables where needed instead of hard-coding release-specific values in the
ConfigMap.

The database path is rendered directly by the StatefulSet because it is coupled to the fixed data
mount. Every supported non-secret application value has a documented chart default matching the
Rust runtime default unless the Kubernetes environment requires a more explicit value.

## Secret model

The chart never creates a Secret. Installation requires the name of one existing Secret. Values
select configurable keys in that Secret for:

- the required `GHE_MASTER_KEY` value;
- the required `GHE_ADMIN_TOKEN` value;
- optional generic OTLP headers;
- optional trace-specific OTLP headers;
- optional log-specific OTLP headers.

The two required key references are always rendered with `optional: false`. OTLP header references
are rendered only when their key names are configured. Secret values never appear in defaults,
examples, annotations, command arguments, NOTES, or generated resources. Documentation uses
shell variable placeholders when demonstrating Secret creation and does not include credential-like
fixture values.

## Security and resources

Defaults align with the production image's fixed identity:

- pod `runAsNonRoot: true`;
- pod `runAsUser`, `runAsGroup`, and `fsGroup` set to `65532`;
- `fsGroupChangePolicy: OnRootMismatch`;
- container `allowPrivilegeEscalation: false`;
- container `readOnlyRootFilesystem: true`;
- all Linux capabilities dropped;
- seccomp profile `RuntimeDefault`.

The data PVC is the only writable filesystem required by the service. The pod security context
makes supported volume providers grant group write access to the mounted storage. The README notes
that a provider which ignores `fsGroup` must provision ownership compatible with UID/GID 65532.

The chart supplies non-empty CPU and memory requests and limits. Values remain configurable but
schema validation requires all four quantities to remain present and non-empty.

No ServiceAccount, host namespace, host path, privileged mode, added capability, or default token
mount is required. The pod disables automatic ServiceAccount token mounting.

## Shutdown contract

Chart values separately configure `GHE_SHUTDOWN_TIMEOUT_SECONDS` and
`GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS`. The pod's `terminationGracePeriodSeconds` must be strictly
greater than their sum, leaving a positive Kubernetes scheduling margin after both application
boundaries.

JSON Schema validates that all three values are positive integers in bounded ranges. Helm template
validation performs the cross-field sum comparison because Helm's supported JSON Schema dialect
cannot express arithmetic relationships. Invalid combinations fail `helm lint` and `helm template`
with an actionable error naming the three values. The pod has no default `preStop` sleep.

## Pod disruption budget

An optional `policy/v1` PodDisruptionBudget is disabled by default. When enabled it selects exactly
the StatefulSet pod and fixes `minAvailable: 0`. No value permits a stricter minimum because a
singleton cannot promise availability during voluntary disruption and a positive minimum could
block intended maintenance.

## Validation strategy

A focused shell test suite renders the chart with Helm and inspects YAML structurally with `yq`.
The suite covers defaults and representative overrides and verifies:

- chart lint and template success;
- exactly one StatefulSet replica and rejection of every tested non-one value;
- `RollingUpdate`, PVC template, `ReadWriteOnce`, mount, and database path;
- ClusterIP Service ports and selectors;
- liveness and readiness paths;
- resource requests and limits;
- pod and container security contexts;
- required and optional existing-Secret references;
- absence of Secret resources and credential-shaped values;
- ConfigMap projection of supported non-secret values;
- shutdown grace acceptance and rejection around the strict sum boundary;
- optional PDB omission and `minAvailable: 0` behavior;
- schema rejection of invalid storage size, access modes, resource quantities, and bounded integer
  settings.

The test script uses temporary files and exits on the first failed assertion with context. Focused
`justfile` recipes run chart linting and render tests without taking ownership of the broader CI
matrix assigned to #46.

For Kubernetes API acceptance, the rendered defaults are submitted to a disposable Kind cluster.
The test creates a metadata-only existing Secret with valid key names, installs without waiting for
the unpublished image to become ready, verifies that Kubernetes accepted the resource structures,
and uninstalls the release. This proves API acceptance without claiming the lifecycle coverage
assigned to #47.

The standard project gates remain:

- `just fmt`
- `cargo build`
- `cargo clippy --all-targets -- -D warnings`
- `just test`
- `cargo doc --no-deps`

## Documentation

The chart README documents prerequisites, installation, existing-Secret creation, values, local
render validation, storage ownership, probe semantics, shutdown sizing, PDB limitations, image tag
overrides, and the future safe-rollout procedure. `docs/operations.md` links to the chart and
summarizes its supported singleton contract. A timestamped changelog entry records the iteration.

## Explicit exclusions

This change does not add Ingress, NetworkPolicy, ServiceMonitor, separate administrative routing,
image or chart publication, backup and restore procedures, multi-replica support, shared-write
storage, an in-cluster collector, or full lifecycle tests.
