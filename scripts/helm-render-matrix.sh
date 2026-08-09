#!/usr/bin/env bash
set -Eeuo pipefail

readonly CHART_DIRECTORY="${1:-}"
readonly OUTPUT_DIRECTORY="${2:-}"
SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY
readonly CASES_FILE="${SCRIPT_DIRECTORY}/../ci/helm/render-cases.txt"
readonly VALUES_DIRECTORY="${SCRIPT_DIRECTORY}/../ci/helm/values"
readonly OUTPUT_GUARD="${SCRIPT_DIRECTORY}/helm-output-directory.sh"
readonly REPOSITORY_ROOT="${SCRIPT_DIRECTORY}/.."
readonly HELM_RELEASE_NAME="github-webhook-exporter"
readonly KUBE_VERSION="1.31.0"
export HELM_OUTPUT_ERROR_PREFIX="Helm render matrix"

declare -A SUPPORTED_CASES=(
    [default]=1
    [persistence]=1
    [external-secret]=1
    [otlp]=1
    [pdb]=1
    [webhook-ingress]=1
    [metrics]=1
    [administration]=1
    [network-policy-default-deny]=1
    [network-policy-bounded]=1
)

fail() {
    printf 'Helm render matrix failed: %s\n' "$1" >&2
    exit 1
}

if [[ -z "${CHART_DIRECTORY}" || -z "${OUTPUT_DIRECTORY}" ]]; then
    printf 'usage: %s CHART_DIRECTORY OUTPUT_DIRECTORY\n' "${0##*/}" >&2
    exit 2
fi

for command in helm mkdir mktemp mv python3 rm rmdir yq; do
    if ! command -v "${command}" >/dev/null 2>&1; then
        printf 'required command not found: %s\n' "${command}" >&2
        exit 2
    fi
done

if [[ ! -f "${CASES_FILE}" ]]; then
    fail "missing render matrix contract: ${CASES_FILE}"
fi
if [[ ! -f "${OUTPUT_GUARD}" ]]; then
    fail "missing output-directory guard"
fi
# shellcheck source=helm-output-directory.sh disable=SC1091
source "${OUTPUT_GUARD}"
helm_output_prepare "helm-render-matrix" "${CHART_DIRECTORY}" "${OUTPUT_DIRECTORY}" \
    "${REPOSITORY_ROOT}"

TEMPORARY_DIRECTORY="$(mktemp -d)"
readonly TEMPORARY_DIRECTORY
cleanup() {
    rm -rf -- "${TEMPORARY_DIRECTORY}"
    helm_output_cleanup_stage
}
trap cleanup EXIT

readarray -t RENDER_CASES <"${CASES_FILE}"
declare -A SEEN_CASES=()
declare -a ORDERED_CASES=()

for case_name in "${RENDER_CASES[@]}"; do
    if [[ -z "${case_name}" ]]; then
        fail 'render matrix contract contains a blank case name'
    fi
    if [[ -n "${SEEN_CASES[${case_name}]+x}" ]]; then
        fail "render matrix contract contains a duplicate case name: ${case_name}"
    fi
    if [[ -z "${SUPPORTED_CASES[${case_name}]+x}" ]]; then
        fail "render matrix contract contains an unknown case name: ${case_name}"
    fi
    SEEN_CASES["${case_name}"]=1
    ORDERED_CASES+=("${case_name}")
done

for case_name in "${!SUPPORTED_CASES[@]}"; do
    if [[ -z "${SEEN_CASES[${case_name}]+x}" ]]; then
        fail "render matrix contract is missing required case name: ${case_name}"
    fi
done

for case_name in "${ORDERED_CASES[@]}"; do
    rendered_manifest="${HELM_OUTPUT_STAGE}/${case_name}.yaml"
    if [[ "${case_name}" == default ]]; then
        helm template "${HELM_RELEASE_NAME}" "${CHART_DIRECTORY}" \
            --kube-version "${KUBE_VERSION}" \
            >"${rendered_manifest}"
    else
        values_file="${VALUES_DIRECTORY}/${case_name}.yaml"
        if [[ ! -f "${values_file}" ]]; then
            fail "missing values fixture for case: ${case_name}"
        fi
        helm template "${HELM_RELEASE_NAME}" "${CHART_DIRECTORY}" \
            --kube-version "${KUBE_VERSION}" \
            --values "${values_file}" \
            >"${rendered_manifest}"
    fi

    normalized_manifest="${TEMPORARY_DIRECTORY}/${case_name}.yaml"
    yq eval-all '[.] | flatten | map(select(. != null))' "${rendered_manifest}" \
        >"${normalized_manifest}"

    if ! yq --exit-status \
        '([.[] | select(.kind == "StatefulSet")] | length) == 1 and
         ([.[] | select(.kind == "Secret")] | length) == 0' \
        "${normalized_manifest}" >/dev/null; then
        fail "rendered manifest failed workload or Secret checks for case: ${case_name}"
    fi

    printf '%s\n' "${case_name}"
done

helm_output_commit
