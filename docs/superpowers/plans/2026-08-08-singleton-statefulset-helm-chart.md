# Singleton StatefulSet Helm Chart Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a versioned Helm chart that securely deploys exactly one non-root exporter with persistent SQLite storage and validated lifecycle settings.

**Architecture:** A typed values file and JSON Schema define the supported chart surface. Focused Helm templates render one StatefulSet, one ClusterIP Service, one non-secret ConfigMap, and an optional singleton PDB; strict Bash tests use Helm and `yq` for schema, cross-field, and manifest assertions, while a separate Kind check proves Kubernetes API acceptance.

**Tech Stack:** Helm 4, Kubernetes APIs (`apps/v1`, `v1`, `policy/v1`), JSON Schema, Go templates, Bash, `yq`, Kind, kubectl, just.

## Global Constraints

- Chart path: `charts/github-webhook-exporter/`.
- Chart version and `appVersion`: `0.1.0`.
- Default image: `ghcr.io/petergrace/github-webhook-exporter:0.1.0` via an empty tag falling back to `Chart.appVersion`.
- Support exactly one StatefulSet replica and `ReadWriteOnce` persistent SQLite storage.
- Use UID/GID/fsGroup `65532`, seccomp `RuntimeDefault`, a read-only root filesystem, no privilege escalation, and no Linux capabilities.
- Mount data at `/var/lib/github-webhook-exporter` and default `GHE_DATABASE_PATH` to `/var/lib/github-webhook-exporter/github-webhook-exporter.db`.
- Reference one existing Secret; never create a Secret or include credential values in defaults, examples, rendered resources, or command arguments.
- Keep `/health/live` process-only and `/health/ready` SQLite-aware; neither probe depends on OTLP.
- Require `terminationGracePeriodSeconds > shutdownTimeoutSeconds + telemetry.shutdownTimeoutSeconds` and add no `preStop` sleep.
- Keep the PDB optional, with fixed `minAvailable: 0` when enabled.
- Do not add Ingress, NetworkPolicy, ServiceMonitor, image publication, chart publication, backup/restore, or multi-replica support.

---

### Task 1: Chart contract and schema rejection tests

**Files:**
- Create: `charts/github-webhook-exporter/Chart.yaml`
- Create: `charts/github-webhook-exporter/values.yaml`
- Create: `charts/github-webhook-exporter/values.schema.json`
- Create: `scripts/helm-chart-test.sh`
- Modify: `justfile`

**Interfaces:**
- Consumes: Helm, `yq`, and the runtime defaults in `src/config.rs`.
- Produces: `just helm-lint` and `just helm-test`; values rooted at `replicaCount`, `image`, `existingSecret`, `service`, `application`, `retention`, `telemetry`, `persistence`, `resources`, `probes`, `terminationGracePeriodSeconds`, and `podDisruptionBudget`.

- [ ] **Step 1: Write the failing chart test harness**

Create `scripts/helm-chart-test.sh` with `set -Eeuo pipefail`, required-command checks for `helm`,
`mktemp`, `rm`, and `yq`, a temporary-directory cleanup trap, and these helpers:

```bash
fail() {
    printf 'Helm chart check failed: %s\n' "$1" >&2
    exit 1
}

expect_failure() {
    local description="$1"
    shift
    if "$@" >"${TEMPORARY_DIRECTORY}/unexpected-output" 2>&1; then
        fail "${description} unexpectedly succeeded"
    fi
}

assert_yq() {
    local expression="$1"
    local file="$2"
    local description="$3"
    yq --exit-status "${expression}" "${file}" >/dev/null || fail "${description}"
}
```

Start the script by running default `helm lint` and `helm template` into
`${TEMPORARY_DIRECTORY}/default.yaml`, then assert that exactly one StatefulSet document exists.
This keeps the harness RED after metadata exists but workload templates do not. Add negative
invocations requiring rejection of:

```text
--set replicaCount=0
--set replicaCount=2
--set persistence.size=
--set persistence.accessModes[0]=ReadWriteMany
--set terminationGracePeriodSeconds=35
--set resources.requests.cpu=
--set resources.limits.memory=
```

Add a `justfile` chart path variable and recipes:

```make
helm-chart := "charts/github-webhook-exporter"

helm-lint:
    helm lint "{{helm-chart}}"

helm-test:
    scripts/helm-chart-test.sh "{{helm-chart}}"
```

- [ ] **Step 2: Verify RED before creating the chart**

Run:

```bash
bash -n scripts/helm-chart-test.sh
just helm-test
```

Expected: Bash syntax passes; `just helm-test` fails because `Chart.yaml` does not exist.

- [ ] **Step 3: Add chart metadata and typed defaults**

Create `Chart.yaml` with `apiVersion: v2`, type `application`, name and description, version `0.1.0`,
and quoted `appVersion: "0.1.0"`.

Create `values.yaml` with these exact supported defaults:

```yaml
replicaCount: 1
image:
  repository: ghcr.io/petergrace/github-webhook-exporter
  tag: ""
  pullPolicy: IfNotPresent
existingSecret:
  name: github-webhook-exporter
  keys:
    masterKey: master-key
    adminToken: admin-token
    otlpHeaders: ""
    otlpTracesHeaders: ""
    otlpLogsHeaders: ""
service:
  type: ClusterIP
  port: 8080
application:
  bindAddress: "[::]:8080"
  shutdownTimeoutSeconds: 30
  webhookBodyLimitBytes: 2097152
  workflowJobMaxSteps: 256
  rustLog: info
retention:
  deliveryDays: 7
  mergeQueueDays: 90
  pruneIntervalSeconds: 3600
telemetry:
  endpoint: ""
  tracesEndpoint: ""
  logsEndpoint: ""
  timeoutMilliseconds: 10000
  tracesTimeoutMilliseconds: null
  logsTimeoutMilliseconds: null
  serviceName: github-webhook-exporter
  queueCapacity: 2048
  batchSize: 512
  shutdownTimeoutSeconds: 5
persistence:
  storageClass: null
  accessModes:
    - ReadWriteOnce
  size: 1Gi
resources:
  requests:
    cpu: 50m
    memory: 64Mi
  limits:
    cpu: 500m
    memory: 256Mi
probes:
  liveness:
    initialDelaySeconds: 5
    periodSeconds: 10
    timeoutSeconds: 2
    failureThreshold: 3
  readiness:
    initialDelaySeconds: 2
    periodSeconds: 5
    timeoutSeconds: 2
    failureThreshold: 3
terminationGracePeriodSeconds: 40
podDisruptionBudget:
  enabled: false
```

- [ ] **Step 4: Add JSON Schema constraints**

Create a draft-07 `values.schema.json` with `additionalProperties: false` at every object level.
Require every top-level group above. Use `const: 1` for `replicaCount`; enums for `IfNotPresent`,
`Always`, `Never`, `ClusterIP`, and `ReadWriteOnce`; `minLength: 1` for the image repository,
existing Secret name, required Secret keys, bind address, Rust filter, and telemetry service name.
Require all four resource quantities to match the same Kubernetes quantity pattern used for PVC
size. Permit optional OTLP header key strings to be empty.

Constrain positive integer settings with `minimum: 1`; cap service port at 65535,
`webhookBodyLimitBytes` at 2097152, and `workflowJobMaxSteps` at 1024. Require telemetry batch size
and queue capacity as positive integers; their ordering remains a template check. Model signal
timeouts as either `null` or a positive integer. Require persistence access modes to contain exactly
one item equal to `ReadWriteOnce`, and require size to match a Kubernetes quantity pattern such as
`^[0-9]+(?:\.[0-9]+)?(?:[EPTGMK]i?|[eE][+-]?[0-9]+)?$`.

- [ ] **Step 5: Verify schema behavior reaches the next RED state**

Run:

```bash
just helm-test
```

Expected: invalid replicas, storage, and empty resources are rejected. The test remains RED because
the default StatefulSet is absent and cross-field shutdown validation is not implemented. If Helm
permits a fixture intended for schema rejection, tighten the corresponding schema before continuing.

- [ ] **Step 6: Commit the tested chart contract**

```bash
git add charts/github-webhook-exporter/Chart.yaml \
    charts/github-webhook-exporter/values.yaml \
    charts/github-webhook-exporter/values.schema.json \
    scripts/helm-chart-test.sh justfile
git commit -m "test: define singleton Helm chart contracts"
```

### Task 2: Core StatefulSet, Service, and non-secret configuration

**Files:**
- Create: `charts/github-webhook-exporter/templates/_helpers.tpl`
- Create: `charts/github-webhook-exporter/templates/configmap.yaml`
- Create: `charts/github-webhook-exporter/templates/service.yaml`
- Create: `charts/github-webhook-exporter/templates/statefulset.yaml`
- Modify: `scripts/helm-chart-test.sh`

**Interfaces:**
- Consumes: the typed values from Task 1 and existing Secret keys under `.Values.existingSecret.keys`.
- Produces: helpers `github-webhook-exporter.name`, `.fullname`, `.chart`, `.labels`, `.selectorLabels`, and `.validate`; one ConfigMap, one ClusterIP Service, and one singleton StatefulSet.

- [ ] **Step 1: Add failing default-render assertions**

After rendering defaults, split manifest assertions by selecting documents with `yq`. Require:

```text
StatefulSet count == 1
StatefulSet spec.replicas == 1
StatefulSet spec.updateStrategy.type == "RollingUpdate"
StatefulSet spec.podManagementPolicy == "OrderedReady"
Service count == 1 and spec.type == "ClusterIP"
ConfigMap count == 1
Secret count == 0
```

Assert stable selector equality between the Service and StatefulSet pod labels, image
`ghcr.io/petergrace/github-webhook-exporter:0.1.0`, port 8080, mount path
`/var/lib/github-webhook-exporter`, and database path
`/var/lib/github-webhook-exporter/github-webhook-exporter.db`.

Run `just helm-test` and expect failure because templates are absent.

- [ ] **Step 2: Implement stable naming and validation helpers**

In `_helpers.tpl`, bound generated names to 63 characters, trim trailing hyphens, and define common
labels containing `helm.sh/chart`, `app.kubernetes.io/name`, `app.kubernetes.io/instance`,
`app.kubernetes.io/version`, and `app.kubernetes.io/managed-by`. Keep selector labels limited to
name and instance.

Define `github-webhook-exporter.validate` and invoke it at the beginning of the StatefulSet. Fail
unless replica count equals one, persistence has exactly `ReadWriteOnce`, telemetry batch size is no
greater than queue capacity, and termination grace is strictly greater than the sum of application
and telemetry shutdown seconds. Use messages naming the rejected values.

- [ ] **Step 3: Implement the non-secret ConfigMap**

Render string values for these keys:

```text
GHE_BIND_ADDRESS
GHE_SHUTDOWN_TIMEOUT_SECONDS
GHE_WEBHOOK_BODY_LIMIT_BYTES
GHE_WORKFLOW_JOB_MAX_STEPS
GHE_DELIVERY_RETENTION_DAYS
GHE_MERGE_QUEUE_RETENTION_DAYS
GHE_DELIVERY_PRUNE_INTERVAL_SECONDS
RUST_LOG
GHE_OTEL_QUEUE_CAPACITY
GHE_OTEL_BATCH_SIZE
GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS
OTEL_EXPORTER_OTLP_TIMEOUT
OTEL_SERVICE_NAME
```

Conditionally render generic, trace, and log OTLP endpoints and signal timeout keys only when their
values are non-empty/non-null. Do not render any header key or Secret value in the ConfigMap.

- [ ] **Step 4: Implement the ClusterIP Service**

Render one `v1/Service`, applying common labels and stable selectors. Set the configurable service
type, expose `.Values.service.port`, and target a named container port `http`.

- [ ] **Step 5: Implement the singleton StatefulSet**

Render one `apps/v1/StatefulSet` with one replica, `RollingUpdate`, `OrderedReady`, stable selectors,
and `automountServiceAccountToken: false`. Render `storageClassName` whenever the configured value is
not `null`, preserving an explicit empty string rather than treating it as absent. Set pod security
context to UID/GID/fsGroup 65532,
`fsGroupChangePolicy: OnRootMismatch`, and seccomp `RuntimeDefault`. Set container security context
to non-root, no privilege escalation, read-only root filesystem, and `capabilities.drop: [ALL]`.

Use ConfigMap `envFrom`; render direct environment entries for database path, required Secret keys,
optional header Secret keys, `POD_NAME`, `POD_NAMESPACE`, and:

```yaml
- name: OTEL_RESOURCE_ATTRIBUTES
  value: "k8s.pod.name=$(POD_NAME),k8s.namespace.name=$(POD_NAMESPACE)"
```

Render exact HTTP liveness/readiness paths, configured resources, no lifecycle/preStop block, and a
PVC template with optional `storageClassName`, `ReadWriteOnce`, and requested storage size.

- [ ] **Step 6: Verify defaults and representative overrides are GREEN**

Extend the test to render an override with explicit image tag, storage class, 2Gi storage, OTLP
endpoints, all optional header key names, and changed resource/probe values. Assert each override is
present, each Secret reference points to the single configured Secret, and no `kind: Secret` exists.

Run:

```bash
just helm-lint
just helm-test
```

Expected: both commands pass; defaults and representative overrides satisfy every structural
assertion.

- [ ] **Step 7: Commit the core workload**

```bash
git add charts/github-webhook-exporter/templates scripts/helm-chart-test.sh
git commit -m "feat: add singleton StatefulSet Helm templates"
```

### Task 3: PDB, negative boundaries, and credential hygiene

**Files:**
- Create: `charts/github-webhook-exporter/templates/pdb.yaml`
- Create: `charts/github-webhook-exporter/templates/NOTES.txt`
- Modify: `scripts/helm-chart-test.sh`

**Interfaces:**
- Consumes: stable selector helpers and `.Values.podDisruptionBudget.enabled`.
- Produces: no PDB by default; exactly one `policy/v1` PDB with `minAvailable: 0` when enabled; non-sensitive installation notes.

- [ ] **Step 1: Add failing PDB and boundary assertions**

Require zero PDB documents by default. Render with `--set podDisruptionBudget.enabled=true` and
require one PDB, integer `minAvailable: 0`, and selectors equal to the StatefulSet selectors.

Add negative render tests for grace equal to the shutdown sum, grace below the sum, telemetry batch
size greater than queue capacity, empty existing Secret name, empty required key names, zero probe
values, and invalid service ports. Add a positive boundary where grace is exactly one second above
the sum.

Scan every rendered fixture for `changeme`, `replace-me`, `example-token`, `example-master-key`,
`authorization=`, and `kind: Secret`; fail if any appears.

Run `just helm-test` and expect PDB-enabled assertions to fail because the template is absent.

- [ ] **Step 2: Implement the optional PDB**

Render `policy/v1` only when enabled. Reuse common and selector labels, fix `minAvailable: 0`, and do
not expose `minAvailable` or `maxUnavailable` as user values.

- [ ] **Step 3: Add safe NOTES output**

Print the release Service DNS name, live and ready paths, existing Secret name, and a reminder that
one replica is mandatory. Do not print Secret keys as values, credentials, OTLP headers, or a shell
command containing credential placeholders.

- [ ] **Step 4: Verify all chart contracts**

Run:

```bash
bash -n scripts/helm-chart-test.sh
shellcheck scripts/helm-chart-test.sh
just helm-lint
just helm-test
```

Expected: syntax, ShellCheck, lint, defaults, overrides, negative fixtures, PDB behavior, and
credential scans all pass.

- [ ] **Step 5: Commit disruption and security coverage**

```bash
git add charts/github-webhook-exporter/templates/pdb.yaml \
    charts/github-webhook-exporter/templates/NOTES.txt \
    scripts/helm-chart-test.sh
git commit -m "test: cover Helm security and lifecycle boundaries"
```

### Task 4: Kubernetes API acceptance in Kind

**Files:**
- Create: `scripts/helm-kind-acceptance.sh`
- Modify: `justfile`

**Interfaces:**
- Consumes: Docker, Kind, kubectl, Helm, and the complete chart.
- Produces: `just helm-kind-acceptance`, which creates an isolated cluster, installs the chart far enough for API acceptance, verifies objects, and cleans up.

- [ ] **Step 1: Write the Kind acceptance script**

Create a strict Bash script that validates its chart argument and required commands (`base64`,
`head`, `helm`, `kind`, `kubectl`, `mktemp`, and `rm`); derives a collision-resistant cluster name;
and installs an EXIT trap that uninstalls the release when possible and deletes only the cluster it
created.

The script must:

1. create a Kind cluster with `--wait 60s`;
2. create namespace `github-webhook-exporter-test`;
3. create Secret `github-webhook-exporter` from literal values generated at runtime from
   `/dev/urandom`, never committed or printed;
4. run `helm install github-webhook-exporter CHART --namespace ... --wait=false`;
5. use `kubectl rollout pause` immediately to avoid treating image availability as acceptance;
6. require the StatefulSet, Service, ConfigMap, and PVC to exist through API reads;
7. require StatefulSet replicas to equal one and the pod security UID to equal 65532;
8. run `helm uninstall` and confirm the release is absent.

Add:

```make
helm-kind-acceptance:
    scripts/helm-kind-acceptance.sh "{{helm-chart}}"
```

- [ ] **Step 2: Verify RED with an intentionally invalid chart path**

Run:

```bash
bash -n scripts/helm-kind-acceptance.sh
scripts/helm-kind-acceptance.sh charts/does-not-exist
```

Expected: syntax passes and the script exits nonzero before creating a cluster because the chart
path is invalid.

- [ ] **Step 3: Exercise the real chart against Kubernetes**

Run:

```bash
shellcheck scripts/helm-kind-acceptance.sh
just helm-kind-acceptance
```

Expected: Kind accepts every object, assertions pass without waiting for the GHCR image, the Helm
release is removed, and the disposable cluster is deleted.

- [ ] **Step 4: Commit API acceptance coverage**

```bash
git add scripts/helm-kind-acceptance.sh justfile
git commit -m "test: verify Helm chart Kubernetes acceptance"
```

### Task 5: Chart and operator documentation

**Files:**
- Create: `charts/github-webhook-exporter/README.md`
- Modify: `docs/operations.md`
- Create: `changelog/2026-08-07T22-06-52-0400-singleton-statefulset-helm-chart.md`

**Interfaces:**
- Consumes: final chart values, templates, and validation recipes.
- Produces: installation, configuration, storage, security, lifecycle, and validation guidance for operators.

- [ ] **Step 1: Write the chart README**

Document prerequisites, the default GHCR repository and `appVersion` tag behavior, and installation
using one existing Secret. Demonstrate Secret creation by reading `GHE_MASTER_KEY` and
`GHE_ADMIN_TOKEN` from the operator's shell environment with `kubectl create secret generic`; do
not provide sample credential values.

Include a complete values table and explain:

- fixed singleton and `ReadWriteOnce` constraints;
- storage class null versus empty-string behavior;
- UID/GID/fsGroup 65532 and providers that ignore fsGroup;
- live versus ready probe semantics and OTLP independence;
- ConfigMap versus existing-Secret projection;
- strict shutdown sum and absence of preStop delay;
- optional `minAvailable: 0` PDB limitations;
- `RollingUpdate` plus the Recreate-equivalent operator procedure deferred to #48;
- `just helm-lint`, `just helm-test`, and `just helm-kind-acceptance`.

- [ ] **Step 2: Extend service operations documentation**

Add a Helm deployment section to `docs/operations.md` linking the chart README and summarizing the
supported singleton, PVC, non-root, probes, Secret reference, termination-grace, and image-tag
contracts. State that #50 supplies GHCR publication and that users may override image repository/tag
until the required release exists.

- [ ] **Step 3: Add the timestamped changelog**

Record the versioned chart, singleton StatefulSet/PVC, Service/probes, non-root security, typed
configuration, existing-Secret model, strict lifecycle checks, PDB, render tests, and Kind API
acceptance. Do not claim Ingress, NetworkPolicy, image publication, or lifecycle behavior testing.

- [ ] **Step 4: Validate documentation and chart packaging surfaces**

Run:

```bash
helm show chart charts/github-webhook-exporter
helm show values charts/github-webhook-exporter
helm template documentation-check charts/github-webhook-exporter >/tmp/ghe-chart.yaml
rg -n 'changeme|replace-me|example-token|example-master-key|authorization=' \
    charts/github-webhook-exporter docs/operations.md /tmp/ghe-chart.yaml
```

Expected: Helm displays valid metadata and values; rendering succeeds; `rg` returns exit status 1
with no credential-shaped matches.

- [ ] **Step 5: Commit operator documentation**

```bash
git add charts/github-webhook-exporter/README.md docs/operations.md \
    changelog/2026-08-07T22-06-52-0400-singleton-statefulset-helm-chart.md
git commit -m "docs: document singleton Helm operations"
```

### Task 6: Full validation and pull-request delivery

**Files:**
- No additional repository files.

**Interfaces:**
- Consumes: Tasks 1 through 5 and issue #44.
- Produces: a validated branch, pull request against `main`, and explicit issue timeline link.

- [ ] **Step 1: Run the mandatory validation sequence**

Run from the repository root in this exact order:

```bash
just fmt
cargo build
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps
just helm-lint
just helm-test
just helm-kind-acceptance
git diff --check origin/main...HEAD
```

Expected: every command exits zero; Rust emits no warnings; chart defaults, overrides, negative
fixtures, credential scans, and Kubernetes API acceptance all pass. If any command fails, fix the
cause and restart the sequence from `just fmt`.

- [ ] **Step 2: Verify repository state and commits**

Run:

```bash
git status --short
git log --oneline origin/main..HEAD
```

Expected: a clean working tree and focused design, plan, contract-test, implementation, acceptance,
and documentation commits.

- [ ] **Step 3: Push and open the pull request**

Push `feat-issue-44-singleton-helm-chart` and open a PR against `main` titled
`feat: add the singleton StatefulSet Helm chart`. Include the actual validation evidence, explain
that GHCR publishing is tracked by #50, and include `Closes #44`.

- [ ] **Step 4: Link the pull request to issue #44**

Comment on #44 with the PR number and URL returned by `gh pr create`.
