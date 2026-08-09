#!/usr/bin/env bash
set -Eeuo pipefail

readonly RENDERED_DIRECTORY="${1:-}"
SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY
readonly SCHEMA_DIRECTORY="${SCRIPT_DIRECTORY}/../ci/helm/schemas"
readonly UNSUPPORTED_FIXTURE="${SCRIPT_DIRECTORY}/../ci/helm/negative/schema/unsupported-api.yaml"
readonly SERVICEMONITOR_TYPO_FIXTURE="${SCRIPT_DIRECTORY}/../ci/helm/negative/schema/servicemonitor-top-level-typo.yaml"
readonly KUBERNETES_VERSIONS=("1.31.0" "1.35.0")
readonly CUSTOM_SCHEMA_LOCATION="${SCHEMA_DIRECTORY}/monitoring.coreos.com/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json"

fail() {
    printf 'Helm kubeconform failed: %s\n' "$1" >&2
    exit 1
}

if [[ -z "${RENDERED_DIRECTORY}" ]]; then
    printf 'usage: %s RENDERED_DIRECTORY\n' "${0##*/}" >&2
    exit 2
fi

for command in grep kubeconform; do
    if ! command -v "${command}" >/dev/null 2>&1; then
        printf 'required command not found: %s\n' "${command}" >&2
        exit 2
    fi
done

if [[ ! -d "${RENDERED_DIRECTORY}" ]]; then
    fail "missing rendered manifest directory"
fi
if [[ ! -d "${SCHEMA_DIRECTORY}" ]]; then
    fail "missing schema directory"
fi
if [[ ! -f "${UNSUPPORTED_FIXTURE}" || ! -f "${SERVICEMONITOR_TYPO_FIXTURE}" ]]; then
    fail "missing negative schema fixture"
fi

kubeconform_arguments=(
    -strict
    -summary
    -kubernetes-version 1.31.0
    -schema-location "${SCHEMA_DIRECTORY}"
    -schema-location "${CUSTOM_SCHEMA_LOCATION}"
)

for kubernetes_version in "${KUBERNETES_VERSIONS[@]}"; do
    printf 'Validating rendered Helm manifests against Kubernetes %s\n' "${kubernetes_version}"
    if ! kubeconform \
        -strict \
        -summary \
        -output pretty \
        -kubernetes-version "${kubernetes_version}" \
        -schema-location "${SCHEMA_DIRECTORY}" \
        -schema-location "${CUSTOM_SCHEMA_LOCATION}" \
        "${RENDERED_DIRECTORY}"; then
        fail "rendered manifests failed validation against Kubernetes ${kubernetes_version}"
    fi
done

if unsupported_output="$(kubeconform "${kubeconform_arguments[@]}" -output json \
    "${UNSUPPORTED_FIXTURE}" 2>&1)"; then
    fail 'unsupported API fixture unexpectedly validated'
fi
if ! grep -Fq 'Ingress' <<<"${unsupported_output}" ||
   ! grep -Fq 'extensions/v1beta1' <<<"${unsupported_output}" ||
   ! grep -Fq 'could not find schema for Ingress' <<<"${unsupported_output}"; then
    fail 'unsupported API rejection lost its expected diagnostic'
fi
printf 'Unsupported API fixture rejected as expected: extensions/v1beta1 Ingress\n'

if typo_output="$(kubeconform "${kubeconform_arguments[@]}" -output json \
    "${SERVICEMONITOR_TYPO_FIXTURE}" 2>&1)"; then
    fail 'ServiceMonitor top-level typo unexpectedly validated'
fi
if ! grep -Fq 'additional properties' <<<"${typo_output}" ||
   ! grep -Fq 'spce' <<<"${typo_output}"; then
    fail 'ServiceMonitor typo rejection lost its strict-root diagnostic'
fi
printf 'ServiceMonitor top-level typo rejected as expected\n'
