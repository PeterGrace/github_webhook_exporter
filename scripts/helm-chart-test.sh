#!/usr/bin/env bash
set -Eeuo pipefail

readonly CHART_DIRECTORY="${1:-}"

if [[ -z "${CHART_DIRECTORY}" ]]; then
    printf 'usage: %s CHART_DIRECTORY\n' "${0##*/}" >&2
    exit 2
fi

for command in awk cat find grep helm mktemp rm yq; do
    if ! command -v "${command}" >/dev/null 2>&1; then
        printf 'required command not found: %s\n' "${command}" >&2
        exit 2
    fi
done

TEMPORARY_DIRECTORY="$(mktemp -d)"
readonly TEMPORARY_DIRECTORY

cleanup() {
    rm -rf "${TEMPORARY_DIRECTORY}"
}
trap cleanup EXIT

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

expect_failure_contains() {
    local description="$1"
    local expected_diagnostic="$2"
    shift 2
    if "$@" >"${TEMPORARY_DIRECTORY}/unexpected-output" 2>&1; then
        fail "${description} unexpectedly succeeded"
    fi
    if ! grep -Fq -- "${expected_diagnostic}" "${TEMPORARY_DIRECTORY}/unexpected-output"; then
        fail "${description} did not emit the expected diagnostic: ${expected_diagnostic}"
    fi
}

expect_success() {
    local description="$1"
    shift
    if ! "$@" >"${TEMPORARY_DIRECTORY}/unexpected-output" 2>&1; then
        fail "${description} unexpectedly failed"
    fi
}

assert_yq() {
    local expression="$1"
    local file="$2"
    local description="$3"
    yq --exit-status "${expression}" "${file}" >/dev/null || fail "${description}"
}

assert_no_sensitive_content() {
    local file="$1"
    if grep -Eq \
        'changeme|replace-me|example-token|example-master-key|authorization=|kind: Secret' \
        "${file}"; then
        fail "rendered fixture ${file##*/} contains credentials, placeholders, or a Secret"
    fi
}

assert_contains() {
    local expected="$1"
    local file="$2"
    local description="$3"
    grep -Fq -- "${expected}" "${file}" || fail "${description}"
}

assert_not_contains() {
    local forbidden="$1"
    local file="$2"
    local description="$3"
    if grep -Eq "${forbidden}" "${file}"; then
        fail "${description}"
    fi
}

helm lint "${CHART_DIRECTORY}"
helm template github-webhook-exporter "${CHART_DIRECTORY}" \
    >"${TEMPORARY_DIRECTORY}/default.yaml"
yq eval-all '[.] | flatten | map(select(. != null))' \
    "${TEMPORARY_DIRECTORY}/default.yaml" >"${TEMPORARY_DIRECTORY}/default-manifests.yaml"

assert_yq \
    '[.[] | select(.kind == "StatefulSet")] | length == 1' \
    "${TEMPORARY_DIRECTORY}/default-manifests.yaml" \
    'defaults must render exactly one StatefulSet'
assert_yq \
    '[.[] | select(.kind == "Service")] | length == 1' \
    "${TEMPORARY_DIRECTORY}/default-manifests.yaml" \
    'defaults must render exactly one Service'
assert_yq \
    '[.[] | select(.kind == "ConfigMap")] | length == 1' \
    "${TEMPORARY_DIRECTORY}/default-manifests.yaml" \
    'defaults must render exactly one ConfigMap'
assert_yq \
    '[.[] | select(.kind == "Secret")] | length == 0' \
    "${TEMPORARY_DIRECTORY}/default-manifests.yaml" \
    'defaults must not render a Secret'
assert_yq \
    '[.[] | select(.kind == "PodDisruptionBudget")] | length == 0' \
    "${TEMPORARY_DIRECTORY}/default-manifests.yaml" \
    'defaults must not render a PodDisruptionBudget'

yq '.[] | select(.kind == "StatefulSet")' \
    "${TEMPORARY_DIRECTORY}/default-manifests.yaml" \
    >"${TEMPORARY_DIRECTORY}/default-statefulset.yaml"
yq '.[] | select(.kind == "Service")' \
    "${TEMPORARY_DIRECTORY}/default-manifests.yaml" \
    >"${TEMPORARY_DIRECTORY}/default-service.yaml"
yq '.[] | select(.kind == "ConfigMap")' \
    "${TEMPORARY_DIRECTORY}/default-manifests.yaml" \
    >"${TEMPORARY_DIRECTORY}/default-configmap.yaml"

assert_yq \
    '.spec.replicas == 1 and .spec.updateStrategy.type == "RollingUpdate" and
     .spec.podManagementPolicy == "OrderedReady"' \
    "${TEMPORARY_DIRECTORY}/default-statefulset.yaml" \
    'StatefulSet must use singleton ordered rolling-update semantics'
assert_yq \
    '(.metadata.labels | length) == 5 and
     .metadata.labels."helm.sh/chart" == "github-webhook-exporter-0.1.0" and
     .metadata.labels."app.kubernetes.io/name" == "github-webhook-exporter" and
     .metadata.labels."app.kubernetes.io/instance" == "github-webhook-exporter" and
     .metadata.labels."app.kubernetes.io/version" == "0.1.0" and
     .metadata.labels."app.kubernetes.io/managed-by" == "Helm"' \
    "${TEMPORARY_DIRECTORY}/default-statefulset.yaml" \
    'resources must receive all standard common labels'
assert_yq \
    '.spec.type == "ClusterIP" and .spec.ports[0].name == "http" and
     .spec.ports[0].port == 8080 and .spec.ports[0].protocol == "TCP" and
     .spec.ports[0].targetPort == "http" and (.spec.ports | length) == 1' \
    "${TEMPORARY_DIRECTORY}/default-service.yaml" \
    'Service must expose the named HTTP port through ClusterIP'
assert_yq \
    '([.[] | select(.kind == "Service")][0].spec.selector | length) == 2 and
     ([.[] | select(.kind == "StatefulSet")][0].spec.selector.matchLabels | length) == 2 and
     ([.[] | select(.kind == "StatefulSet")][0].spec.template.metadata.labels |
       length) == 2 and
     ([.[] | select(.kind == "Service")][0].spec.selector | to_json) ==
     ([.[] | select(.kind == "StatefulSet")][0].spec.selector.matchLabels | to_json) and
     ([.[] | select(.kind == "Service")][0].spec.selector | to_json) ==
     ([.[] | select(.kind == "StatefulSet")][0].spec.template.metadata.labels | to_json)' \
    "${TEMPORARY_DIRECTORY}/default-manifests.yaml" \
    'Service and StatefulSet must share stable pod selector labels'
assert_yq \
    '.spec.template.spec.containers[0].image ==
     "ghcr.io/petergrace/github-webhook-exporter:0.1.0" and
     .spec.template.spec.containers[0].ports[0].name == "http" and
     .spec.template.spec.containers[0].ports[0].containerPort == 8080 and
     .spec.template.spec.containers[0].ports[0].protocol == "TCP" and
     (.spec.template.spec.containers[0].ports | length) == 1' \
    "${TEMPORARY_DIRECTORY}/default-statefulset.yaml" \
    'StatefulSet must use the appVersion image and expose port 8080'
assert_yq \
    '.spec.template.spec.containers[0].volumeMounts[0].name == "data" and
     .spec.template.spec.containers[0].volumeMounts[0].mountPath ==
     "/var/lib/github-webhook-exporter" and
     (.spec.template.spec.containers[0].volumeMounts | length) == 1 and
     ([.spec.template.spec.containers[0].env[] |
       select(.name == "GHE_DATABASE_PATH")][0].value) ==
     "/var/lib/github-webhook-exporter/github-webhook-exporter.db"' \
    "${TEMPORARY_DIRECTORY}/default-statefulset.yaml" \
    'StatefulSet must mount and use the durable SQLite data path'
assert_yq \
    '(.data | length) == 13 and .data.GHE_BIND_ADDRESS == "[::]:8080" and
     .data.GHE_SHUTDOWN_TIMEOUT_SECONDS == "30" and
     .data.GHE_WEBHOOK_BODY_LIMIT_BYTES == "2097152" and
     .data.GHE_WORKFLOW_JOB_MAX_STEPS == "256" and
     .data.GHE_DELIVERY_RETENTION_DAYS == "7" and
     .data.GHE_MERGE_QUEUE_RETENTION_DAYS == "90" and
     .data.GHE_DELIVERY_PRUNE_INTERVAL_SECONDS == "3600" and
     .data.RUST_LOG == "info" and .data.GHE_OTEL_QUEUE_CAPACITY == "2048" and
     .data.GHE_OTEL_BATCH_SIZE == "512" and
     .data.GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS == "5" and
     .data.OTEL_EXPORTER_OTLP_TIMEOUT == "10000" and
     .data.OTEL_SERVICE_NAME == "github-webhook-exporter"' \
    "${TEMPORARY_DIRECTORY}/default-configmap.yaml" \
    'ConfigMap must contain only default non-secret configuration'
assert_yq \
    '.spec.template.metadata.annotations."checksum/config" |
     test("^[0-9a-f]{64}$")' \
    "${TEMPORARY_DIRECTORY}/default-statefulset.yaml" \
    'StatefulSet pod template must contain a SHA-256 ConfigMap checksum'

helm template github-webhook-exporter "${CHART_DIRECTORY}" \
    >"${TEMPORARY_DIRECTORY}/default-repeat.yaml"
helm template github-webhook-exporter "${CHART_DIRECTORY}" \
    --set application.rustLog=debug \
    >"${TEMPORARY_DIRECTORY}/configmap-change.yaml"
default_checksum="$(yq \
    'select(.kind == "StatefulSet").spec.template.metadata.annotations."checksum/config"' \
    "${TEMPORARY_DIRECTORY}/default.yaml")"
repeat_checksum="$(yq \
    'select(.kind == "StatefulSet").spec.template.metadata.annotations."checksum/config"' \
    "${TEMPORARY_DIRECTORY}/default-repeat.yaml")"
changed_checksum="$(yq \
    'select(.kind == "StatefulSet").spec.template.metadata.annotations."checksum/config"' \
    "${TEMPORARY_DIRECTORY}/configmap-change.yaml")"
if [[ "${default_checksum}" != "${repeat_checksum}" ]]; then
    fail 'identical ConfigMaps must produce identical pod-template checksums'
fi
if [[ "${default_checksum}" == "${changed_checksum}" ]]; then
    fail 'changing ConfigMap-backed RUST_LOG must change the pod-template checksum'
fi
assert_yq \
    'select(.kind == "ConfigMap").data.RUST_LOG == "debug"' \
    "${TEMPORARY_DIRECTORY}/configmap-change.yaml" \
    'checksum regression fixture must change ConfigMap-backed RUST_LOG'

helm template github-webhook-exporter "${CHART_DIRECTORY}" \
    --set service.port=9090 \
    >"${TEMPORARY_DIRECTORY}/non-default-port.yaml"
yq eval-all '[.] | flatten | map(select(. != null))' \
    "${TEMPORARY_DIRECTORY}/non-default-port.yaml" \
    >"${TEMPORARY_DIRECTORY}/non-default-port-manifests.yaml"
assert_yq \
    '([.[] | select(.kind == "Service")][0].spec.ports[0].port) == 9090 and
     ([.[] | select(.kind == "StatefulSet")][0].spec.template.spec.containers[0].ports[0]
       .containerPort) == 9090 and
     ([.[] | select(.kind == "ConfigMap")][0].data.GHE_BIND_ADDRESS) == "[::]:9090"' \
    "${TEMPORARY_DIRECTORY}/non-default-port-manifests.yaml" \
    'service.port must configure the Service, container, and application listener together'
assert_yq \
    '.spec.template.spec.automountServiceAccountToken == false and
     .spec.template.spec.securityContext.runAsNonRoot == true and
     .spec.template.spec.securityContext.runAsUser == 65532 and
     .spec.template.spec.securityContext.runAsGroup == 65532 and
     .spec.template.spec.securityContext.fsGroup == 65532 and
     .spec.template.spec.securityContext.fsGroupChangePolicy == "OnRootMismatch" and
     .spec.template.spec.securityContext.seccompProfile.type == "RuntimeDefault" and
     .spec.template.spec.containers[0].securityContext.runAsNonRoot == true and
     .spec.template.spec.containers[0].securityContext.allowPrivilegeEscalation == false and
     .spec.template.spec.containers[0].securityContext.readOnlyRootFilesystem == true and
     .spec.template.spec.containers[0].securityContext.capabilities.drop[0] == "ALL" and
     (.spec.template.spec.containers[0].securityContext.capabilities.drop | length) == 1' \
    "${TEMPORARY_DIRECTORY}/default-statefulset.yaml" \
    'StatefulSet must apply the required pod and container security contexts'
assert_yq \
    '.spec.template.spec.containers[0].livenessProbe.httpGet.path == "/health/live" and
     .spec.template.spec.containers[0].livenessProbe.httpGet.port == "http" and
     .spec.template.spec.containers[0].livenessProbe.httpGet.scheme == "HTTP" and
     .spec.template.spec.containers[0].readinessProbe.httpGet.path == "/health/ready" and
     .spec.template.spec.containers[0].readinessProbe.httpGet.port == "http" and
     .spec.template.spec.containers[0].readinessProbe.httpGet.scheme == "HTTP" and
     (.spec.template.spec.containers[0] | has("lifecycle") | not)' \
    "${TEMPORARY_DIRECTORY}/default-statefulset.yaml" \
    'StatefulSet must use exact HTTP probes and no lifecycle hook'
assert_yq \
    '.spec.volumeClaimTemplates[0].spec.accessModes[0] == "ReadWriteOnce" and
     (.spec.volumeClaimTemplates[0].spec.accessModes | length) == 1 and
     .spec.volumeClaimTemplates[0].spec.resources.requests.storage == "1Gi" and
     (.spec.volumeClaimTemplates[0].spec | has("storageClassName") | not)' \
    "${TEMPORARY_DIRECTORY}/default-statefulset.yaml" \
    'default PVC must use ReadWriteOnce, 1Gi, and the cluster storage class'
# The literal Downward API substitution syntax must reach yq without shell expansion.
# shellcheck disable=SC2016
assert_yq \
    '.spec.template.spec.containers[0].envFrom[0].configMapRef.name ==
     "github-webhook-exporter" and
     (.spec.template.spec.containers[0].envFrom | length) == 1 and
     ([.spec.template.spec.containers[0].env[] |
       select(.name == "GHE_MASTER_KEY")][0].valueFrom.secretKeyRef.name) ==
     "github-webhook-exporter" and
     ([.spec.template.spec.containers[0].env[] |
       select(.name == "GHE_MASTER_KEY")][0].valueFrom.secretKeyRef.key) == "master-key" and
     ([.spec.template.spec.containers[0].env[] |
       select(.name == "GHE_MASTER_KEY")][0].valueFrom.secretKeyRef.optional) == false and
     ([.spec.template.spec.containers[0].env[] |
       select(.name == "GHE_ADMIN_TOKEN")][0].valueFrom.secretKeyRef.name) ==
     "github-webhook-exporter" and
     ([.spec.template.spec.containers[0].env[] |
       select(.name == "GHE_ADMIN_TOKEN")][0].valueFrom.secretKeyRef.key) == "admin-token" and
     ([.spec.template.spec.containers[0].env[] |
       select(.name == "GHE_ADMIN_TOKEN")][0].valueFrom.secretKeyRef.optional) == false and
     ([.spec.template.spec.containers[0].env[] |
       select(.name == "POD_NAME")][0].valueFrom.fieldRef.fieldPath) == "metadata.name" and
     ([.spec.template.spec.containers[0].env[] |
       select(.name == "POD_NAMESPACE")][0].valueFrom.fieldRef.fieldPath) ==
     "metadata.namespace" and
     ([.spec.template.spec.containers[0].env[] |
       select(.name == "OTEL_RESOURCE_ATTRIBUTES")][0].value) ==
     "k8s.pod.name=$(POD_NAME),k8s.namespace.name=$(POD_NAMESPACE)" and
     ([.spec.template.spec.containers[0].env[] |
       select(.name == "OTEL_EXPORTER_OTLP_HEADERS" or
              .name == "OTEL_EXPORTER_OTLP_TRACES_HEADERS" or
              .name == "OTEL_EXPORTER_OTLP_LOGS_HEADERS")] | length) == 0' \
    "${TEMPORARY_DIRECTORY}/default-statefulset.yaml" \
    'StatefulSet must project default ConfigMap, Secret, and downward API environment'

cat >"${TEMPORARY_DIRECTORY}/override-values.yaml" <<'EOF'
image:
  tag: "2.3.4"
existingSecret:
  name: exporter-credentials
  keys:
    masterKey: encryption-key
    adminToken: bearer-token
    otlpHeaders: collector-headers
    otlpTracesHeaders: trace-headers
    otlpLogsHeaders: log-headers
telemetry:
  endpoint: https://collector.example.test:4318
  tracesEndpoint: https://traces.example.test/v1/traces
  logsEndpoint: https://logs.example.test/v1/logs
  tracesTimeoutMilliseconds: 2001
  logsTimeoutMilliseconds: 2002
persistence:
  storageClass: fast-ssd
  size: 2Gi
resources:
  requests:
    cpu: 125m
    memory: 96Mi
  limits:
    cpu: 750m
    memory: 384Mi
probes:
  liveness:
    initialDelaySeconds: 11
    periodSeconds: 12
    timeoutSeconds: 3
    failureThreshold: 4
  readiness:
    initialDelaySeconds: 6
    periodSeconds: 7
    timeoutSeconds: 4
    failureThreshold: 5
EOF

helm template github-webhook-exporter "${CHART_DIRECTORY}" \
    --values "${TEMPORARY_DIRECTORY}/override-values.yaml" \
    >"${TEMPORARY_DIRECTORY}/override.yaml"
yq eval-all '[.] | flatten | map(select(. != null))' \
    "${TEMPORARY_DIRECTORY}/override.yaml" >"${TEMPORARY_DIRECTORY}/override-manifests.yaml"
yq '.[] | select(.kind == "StatefulSet")' \
    "${TEMPORARY_DIRECTORY}/override-manifests.yaml" \
    >"${TEMPORARY_DIRECTORY}/override-statefulset.yaml"
yq '.[] | select(.kind == "ConfigMap")' \
    "${TEMPORARY_DIRECTORY}/override-manifests.yaml" \
    >"${TEMPORARY_DIRECTORY}/override-configmap.yaml"

assert_yq \
    '([.[] | select(.kind == "StatefulSet")] | length) == 1 and
     ([.[] | select(.kind == "Service")] | length) == 1 and
     ([.[] | select(.kind == "ConfigMap")] | length) == 1 and
     ([.[] | select(.kind == "Secret")] | length) == 0' \
    "${TEMPORARY_DIRECTORY}/override-manifests.yaml" \
    'overrides must retain one workload, Service, ConfigMap, and no Secret'
assert_yq \
    '.spec.template.spec.containers[0].image ==
     "ghcr.io/petergrace/github-webhook-exporter:2.3.4" and
     .spec.volumeClaimTemplates[0].spec.storageClassName == "fast-ssd" and
     .spec.volumeClaimTemplates[0].spec.resources.requests.storage == "2Gi"' \
    "${TEMPORARY_DIRECTORY}/override-statefulset.yaml" \
    'image and persistence overrides must be present'
assert_yq \
    '.data.OTEL_EXPORTER_OTLP_ENDPOINT == "https://collector.example.test:4318" and
     .data.OTEL_EXPORTER_OTLP_TRACES_ENDPOINT ==
     "https://traces.example.test/v1/traces" and
     .data.OTEL_EXPORTER_OTLP_LOGS_ENDPOINT == "https://logs.example.test/v1/logs" and
     .data.OTEL_EXPORTER_OTLP_TRACES_TIMEOUT == "2001" and
     .data.OTEL_EXPORTER_OTLP_LOGS_TIMEOUT == "2002" and
     ([.data | keys | .[] | select(test("HEADERS"))] | length) == 0' \
    "${TEMPORARY_DIRECTORY}/override-configmap.yaml" \
    'OTLP endpoint and signal timeout overrides must be present'
assert_yq \
    '.spec.template.spec.containers[0].resources.requests.cpu == "125m" and
     .spec.template.spec.containers[0].resources.requests.memory == "96Mi" and
     .spec.template.spec.containers[0].resources.limits.cpu == "750m" and
     .spec.template.spec.containers[0].resources.limits.memory == "384Mi" and
     .spec.template.spec.containers[0].livenessProbe.initialDelaySeconds == 11 and
     .spec.template.spec.containers[0].livenessProbe.periodSeconds == 12 and
     .spec.template.spec.containers[0].livenessProbe.timeoutSeconds == 3 and
     .spec.template.spec.containers[0].livenessProbe.failureThreshold == 4 and
     .spec.template.spec.containers[0].readinessProbe.initialDelaySeconds == 6 and
     .spec.template.spec.containers[0].readinessProbe.periodSeconds == 7 and
     .spec.template.spec.containers[0].readinessProbe.timeoutSeconds == 4 and
     .spec.template.spec.containers[0].readinessProbe.failureThreshold == 5' \
    "${TEMPORARY_DIRECTORY}/override-statefulset.yaml" \
    'resource and probe overrides must be present'
assert_yq \
    '[([.spec.template.spec.containers[0].env[] |
        select(.name == "GHE_MASTER_KEY")][0].valueFrom.secretKeyRef.name) ==
      "exporter-credentials",
      ([.spec.template.spec.containers[0].env[] |
        select(.name == "GHE_MASTER_KEY")][0].valueFrom.secretKeyRef.key) ==
      "encryption-key",
      ([.spec.template.spec.containers[0].env[] |
        select(.name == "GHE_ADMIN_TOKEN")][0].valueFrom.secretKeyRef.name) ==
      "exporter-credentials",
      ([.spec.template.spec.containers[0].env[] |
        select(.name == "GHE_ADMIN_TOKEN")][0].valueFrom.secretKeyRef.key) == "bearer-token",
      ([.spec.template.spec.containers[0].env[] |
        select(.name == "OTEL_EXPORTER_OTLP_HEADERS")][0].valueFrom.secretKeyRef.name) ==
      "exporter-credentials",
      ([.spec.template.spec.containers[0].env[] |
        select(.name == "OTEL_EXPORTER_OTLP_HEADERS")][0].valueFrom.secretKeyRef.key) ==
      "collector-headers",
      ([.spec.template.spec.containers[0].env[] |
        select(.name == "OTEL_EXPORTER_OTLP_TRACES_HEADERS")][0].valueFrom.secretKeyRef.name) ==
      "exporter-credentials",
      ([.spec.template.spec.containers[0].env[] |
        select(.name == "OTEL_EXPORTER_OTLP_TRACES_HEADERS")][0].valueFrom.secretKeyRef.key) ==
      "trace-headers",
      ([.spec.template.spec.containers[0].env[] |
        select(.name == "OTEL_EXPORTER_OTLP_LOGS_HEADERS")][0].valueFrom.secretKeyRef.name) ==
      "exporter-credentials",
      ([.spec.template.spec.containers[0].env[] |
        select(.name == "OTEL_EXPORTER_OTLP_LOGS_HEADERS")][0].valueFrom.secretKeyRef.key) ==
      "log-headers",
      ([.spec.template.spec.containers[0].env[] |
        select(.valueFrom.secretKeyRef.name == "exporter-credentials") |
        .valueFrom.secretKeyRef.optional] | all_c(. == false))] | all' \
    "${TEMPORARY_DIRECTORY}/override-statefulset.yaml" \
    'every Secret override must reference the configured existing Secret'

helm install exporter "${CHART_DIRECTORY}" \
    --dry-run=client \
    --namespace observability \
    >"${TEMPORARY_DIRECTORY}/dry-run-install.txt"
awk 'found { print } /^NOTES:$/ { found = 1 }' \
    "${TEMPORARY_DIRECTORY}/dry-run-install.txt" >"${TEMPORARY_DIRECTORY}/notes.txt"
assert_contains \
    'exporter-github-webhook-exporter.observability.svc.cluster.local' \
    "${TEMPORARY_DIRECTORY}/notes.txt" \
    'notes must print the release Service DNS name'
assert_contains \
    '/health/live' \
    "${TEMPORARY_DIRECTORY}/notes.txt" \
    'notes must print the liveness path'
assert_contains \
    '/health/ready' \
    "${TEMPORARY_DIRECTORY}/notes.txt" \
    'notes must print the readiness path'
assert_contains \
    'Existing Secret: github-webhook-exporter' \
    "${TEMPORARY_DIRECTORY}/notes.txt" \
    'notes must print the existing Secret name'
assert_contains \
    'one replica is mandatory' \
    "${TEMPORARY_DIRECTORY}/notes.txt" \
    'notes must remind operators that one replica is mandatory'
assert_not_contains \
    'master-key|admin-token|OTEL_EXPORTER_OTLP(_[A-Z]+)?_HEADERS' \
    "${TEMPORARY_DIRECTORY}/notes.txt" \
    'notes must not print Secret key names or OTLP header variables'
assert_no_sensitive_content "${TEMPORARY_DIRECTORY}/dry-run-install.txt"
assert_no_sensitive_content "${TEMPORARY_DIRECTORY}/notes.txt"

helm template github-webhook-exporter "${CHART_DIRECTORY}" \
    --set podDisruptionBudget.enabled=true \
    >"${TEMPORARY_DIRECTORY}/pdb-enabled.yaml"
yq eval-all '[.] | flatten | map(select(. != null))' \
    "${TEMPORARY_DIRECTORY}/pdb-enabled.yaml" \
    >"${TEMPORARY_DIRECTORY}/pdb-enabled-manifests.yaml"
assert_yq \
    '([.[] | select(.kind == "PodDisruptionBudget")] | length) == 1 and
     ([.[] | select(.kind == "PodDisruptionBudget")][0].apiVersion) == "policy/v1" and
     ([.[] | select(.kind == "PodDisruptionBudget")][0].spec.minAvailable) == 0 and
     (([.[] | select(.kind == "PodDisruptionBudget")][0].spec.minAvailable | type) ==
      "!!int") and
     (([.[] | select(.kind == "PodDisruptionBudget")][0].spec | has("maxUnavailable")) | not) and
     (([.[] | select(.kind == "PodDisruptionBudget")][0].metadata.labels | to_json) ==
      ([.[] | select(.kind == "StatefulSet")][0].metadata.labels | to_json)) and
     (([.[] | select(.kind == "PodDisruptionBudget")][0].spec.selector.matchLabels |
       to_json) ==
      ([.[] | select(.kind == "StatefulSet")][0].spec.selector.matchLabels | to_json))' \
    "${TEMPORARY_DIRECTORY}/pdb-enabled-manifests.yaml" \
    'enabled PodDisruptionBudget must use fixed singleton-safe semantics and workload selectors'

helm template github-webhook-exporter "${CHART_DIRECTORY}" \
    --set terminationGracePeriodSeconds=36 \
    >"${TEMPORARY_DIRECTORY}/valid-grace-boundary.yaml"

expect_success \
    'application shutdown maximum of 300 seconds' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" \
        --set application.shutdownTimeoutSeconds=300 \
        --set terminationGracePeriodSeconds=306
expect_failure \
    'application shutdown first value above 300 seconds' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" \
        --set application.shutdownTimeoutSeconds=301 \
        --set terminationGracePeriodSeconds=307
expect_success \
    'telemetry shutdown maximum of 120 seconds' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" \
        --set telemetry.shutdownTimeoutSeconds=120 \
        --set terminationGracePeriodSeconds=151
expect_failure \
    'telemetry shutdown first value above 120 seconds' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" \
        --set telemetry.shutdownTimeoutSeconds=121 \
        --set terminationGracePeriodSeconds=152
expect_success \
    'termination grace maximum of 600 seconds' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" \
        --set terminationGracePeriodSeconds=600
expect_success \
    'combined shutdown maxima preserve a valid strict sum' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" \
        --set application.shutdownTimeoutSeconds=300 \
        --set telemetry.shutdownTimeoutSeconds=120 \
        --set terminationGracePeriodSeconds=600
expect_failure \
    'termination grace first value above 600 seconds' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" \
        --set terminationGracePeriodSeconds=601

readonly -a PROBE_FIELDS=(
    initialDelaySeconds
    periodSeconds
    timeoutSeconds
    failureThreshold
)
readonly -a PROBE_MAXIMUMS=(300 300 60 10)
readonly -a PROBE_FIRST_REJECTED_VALUES=(301 301 61 11)
for probe in liveness readiness; do
    for probe_index in "${!PROBE_FIELDS[@]}"; do
        probe_field="${PROBE_FIELDS[${probe_index}]}"
        probe_maximum="${PROBE_MAXIMUMS[${probe_index}]}"
        probe_first_rejected="${PROBE_FIRST_REJECTED_VALUES[${probe_index}]}"
        expect_success \
            "probes.${probe}.${probe_field} accepted maximum ${probe_maximum}" \
            helm template github-webhook-exporter "${CHART_DIRECTORY}" \
                --set "probes.${probe}.${probe_field}=${probe_maximum}"
        expect_failure \
            "probes.${probe}.${probe_field} first rejected value ${probe_first_rejected}" \
            helm template github-webhook-exporter "${CHART_DIRECTORY}" \
                --set "probes.${probe}.${probe_field}=${probe_first_rejected}"
    done
done

helm template github-webhook-exporter "${CHART_DIRECTORY}" \
    --set-string persistence.storageClass= \
    >"${TEMPORARY_DIRECTORY}/empty-storage-class.yaml"
yq eval-all '[.] | flatten | map(select(.kind == "StatefulSet"))[0]' \
    "${TEMPORARY_DIRECTORY}/empty-storage-class.yaml" \
    >"${TEMPORARY_DIRECTORY}/empty-storage-class-statefulset.yaml"
assert_yq \
    '.spec.volumeClaimTemplates[0].spec |
     (has("storageClassName") and .storageClassName == "")' \
    "${TEMPORARY_DIRECTORY}/empty-storage-class-statefulset.yaml" \
    'explicit empty storage class must render storageClassName as an empty string'

expect_failure \
    'replicaCount=0' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" --set replicaCount=0
expect_failure \
    'replicaCount=2' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" --set replicaCount=2
expect_failure \
    'empty persistence.size' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" --set persistence.size=
expect_failure \
    'malformed non-empty persistence.size' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" \
        --set-string persistence.size=1K
expect_failure \
    'ReadWriteMany persistence access mode' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" \
        --set persistence.accessModes[0]=ReadWriteMany
SHUTDOWN_DIAGNOSTIC='terminationGracePeriodSeconds must be greater than '
SHUTDOWN_DIAGNOSTIC+='application.shutdownTimeoutSeconds + '
SHUTDOWN_DIAGNOSTIC+='telemetry.shutdownTimeoutSeconds; got terminationGracePeriodSeconds='
readonly SHUTDOWN_DIAGNOSTIC
EQUAL_SHUTDOWN_DIAGNOSTIC="${SHUTDOWN_DIAGNOSTIC}35 "
EQUAL_SHUTDOWN_DIAGNOSTIC+='application.shutdownTimeoutSeconds=30 '
EQUAL_SHUTDOWN_DIAGNOSTIC+='telemetry.shutdownTimeoutSeconds=5'
readonly EQUAL_SHUTDOWN_DIAGNOSTIC
BELOW_SHUTDOWN_DIAGNOSTIC="${SHUTDOWN_DIAGNOSTIC}34 "
BELOW_SHUTDOWN_DIAGNOSTIC+='application.shutdownTimeoutSeconds=30 '
BELOW_SHUTDOWN_DIAGNOSTIC+='telemetry.shutdownTimeoutSeconds=5'
readonly BELOW_SHUTDOWN_DIAGNOSTIC
BATCH_DIAGNOSTIC='telemetry.batchSize must be no greater than telemetry.queueCapacity; '
BATCH_DIAGNOSTIC+='got telemetry.batchSize=2049 telemetry.queueCapacity=2048'
readonly BATCH_DIAGNOSTIC

expect_failure_contains \
    'terminationGracePeriodSeconds equal to shutdown timeout sum' \
    "${EQUAL_SHUTDOWN_DIAGNOSTIC}" \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" \
        --set terminationGracePeriodSeconds=35
expect_failure_contains \
    'terminationGracePeriodSeconds below shutdown timeout sum' \
    "${BELOW_SHUTDOWN_DIAGNOSTIC}" \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" \
        --set terminationGracePeriodSeconds=34
expect_failure_contains \
    'telemetry batch size greater than queue capacity' \
    "${BATCH_DIAGNOSTIC}" \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" \
        --set telemetry.batchSize=2049
expect_failure \
    'empty existing Secret name' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" --set existingSecret.name=
expect_failure \
    'empty master key name' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" \
        --set existingSecret.keys.masterKey=
expect_failure \
    'empty admin token key name' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" \
        --set existingSecret.keys.adminToken=
for probe in liveness readiness; do
    for probe_value in initialDelaySeconds periodSeconds timeoutSeconds failureThreshold; do
        expect_failure \
            "zero probes.${probe}.${probe_value}" \
            helm template github-webhook-exporter "${CHART_DIRECTORY}" \
                --set "probes.${probe}.${probe_value}=0"
    done
done
expect_failure \
    'service port below Kubernetes range' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" --set service.port=0
expect_failure \
    'service port above Kubernetes range' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" --set service.port=65536
expect_failure \
    'removed application.bindAddress value' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" \
        --set-string application.bindAddress=127.0.0.1:8080
expect_failure \
    'empty resources.requests.cpu' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" --set resources.requests.cpu=
expect_failure \
    'empty resources.limits.memory' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" --set resources.limits.memory=
for resource_quantity in \
    resources.requests.cpu \
    resources.requests.memory \
    resources.limits.cpu \
    resources.limits.memory; do
    expect_failure \
        "malformed non-empty ${resource_quantity}" \
        helm template github-webhook-exporter "${CHART_DIRECTORY}" \
            --set-string "${resource_quantity}=1K"
done

while IFS= read -r rendered_fixture; do
    assert_no_sensitive_content "${rendered_fixture}"
done < <(find "${TEMPORARY_DIRECTORY}" -type f -name '*.yaml' -print)
