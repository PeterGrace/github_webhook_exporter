#!/usr/bin/env bash
set -Eeuo pipefail

readonly CHART_DIRECTORY="${1:-}"
script_directory="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY="${script_directory}"
readonly RENDER_SCRIPT="${SCRIPT_DIRECTORY}/helm-render-matrix.sh"
readonly SCHEMA_DIRECTORY="${SCRIPT_DIRECTORY}/../ci/helm/schemas"
readonly NEGATIVE_FIXTURE="${SCRIPT_DIRECTORY}/../ci/helm/negative/schema/unsupported-api.yaml"
readonly KUBERNETES_VERSIONS=("1.31.0" "1.35.0")
readonly CUSTOM_SCHEMA_LOCATION="${SCHEMA_DIRECTORY}/monitoring.coreos.com/{{.ResourceKind}}_{{.ResourceAPIVersion}}.json"

fail() {
    printf 'Helm kubeconform failed: %s\n' "$1" >&2
    exit 1
}

if [[ -z "${CHART_DIRECTORY}" ]]; then
    printf 'usage: %s CHART_DIRECTORY\n' "${0##*/}" >&2
    exit 2
fi

for command in helm kubeconform mktemp rm grep; do
    if ! command -v "${command}" >/dev/null 2>&1; then
        printf 'required command not found: %s\n' "${command}" >&2
        exit 2
    fi
done

if [[ ! -x "${RENDER_SCRIPT}" ]]; then
    fail "missing Helm render matrix script: ${RENDER_SCRIPT}"
fi

if [[ ! -d "${SCHEMA_DIRECTORY}" ]]; then
    fail "missing schema directory: ${SCHEMA_DIRECTORY}"
fi

if [[ ! -f "${NEGATIVE_FIXTURE}" ]]; then
    fail "missing negative schema fixture: ${NEGATIVE_FIXTURE}"
fi

temporary_directory="$(mktemp -d)"
readonly TEMPORARY_DIRECTORY="${temporary_directory}"
cleanup() {
    rm -rf "${TEMPORARY_DIRECTORY}"
}
trap cleanup EXIT

readonly RENDER_DIRECTORY="${TEMPORARY_DIRECTORY}/rendered"
mkdir -p "${RENDER_DIRECTORY}"

"${RENDER_SCRIPT}" "${CHART_DIRECTORY}" "${RENDER_DIRECTORY}" >/dev/null

for kubernetes_version in "${KUBERNETES_VERSIONS[@]}"; do
    printf 'Validating rendered Helm manifests against Kubernetes %s\n' "${kubernetes_version}"
    if ! kubeconform \
        -strict \
        -summary \
        -output pretty \
        -kubernetes-version "${kubernetes_version}" \
        -schema-location "${SCHEMA_DIRECTORY}" \
        -schema-location "${CUSTOM_SCHEMA_LOCATION}" \
        "${RENDER_DIRECTORY}"; then
        fail "rendered manifests failed validation against Kubernetes ${kubernetes_version}"
    fi
done

if unsupported_output="$(kubeconform \
    -strict \
    -summary \
    -output json \
    -kubernetes-version 1.31.0 \
    -schema-location "${SCHEMA_DIRECTORY}" \
    -schema-location "${CUSTOM_SCHEMA_LOCATION}" \
    "${NEGATIVE_FIXTURE}" 2>&1)"; then
    fail 'unsupported API fixture unexpectedly validated'
fi

if ! grep -Fq 'Ingress' <<<"${unsupported_output}"; then
    fail 'unsupported API rejection did not mention Ingress'
fi

if ! grep -Fq 'extensions/v1beta1' <<<"${unsupported_output}"; then
    fail 'unsupported API rejection did not mention extensions/v1beta1'
fi

if ! grep -Fq 'could not find schema for Ingress' <<<"${unsupported_output}"; then
    fail 'unsupported API rejection did not report the missing Ingress schema'
fi

printf 'Unsupported API fixture rejected as expected: extensions/v1beta1 Ingress\n'
