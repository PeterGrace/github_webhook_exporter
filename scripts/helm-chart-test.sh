#!/usr/bin/env bash
set -Eeuo pipefail

readonly CHART_DIRECTORY="${1:-}"

if [[ -z "${CHART_DIRECTORY}" ]]; then
    printf 'usage: %s CHART_DIRECTORY\n' "${0##*/}" >&2
    exit 2
fi

for command in helm mktemp rm yq; do
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

assert_yq() {
    local expression="$1"
    local file="$2"
    local description="$3"
    yq --exit-status "${expression}" "${file}" >/dev/null || fail "${description}"
}

helm lint "${CHART_DIRECTORY}"
helm template github-webhook-exporter "${CHART_DIRECTORY}" \
    >"${TEMPORARY_DIRECTORY}/default.yaml"
yq eval-all '[.] | flatten | {
    "statefulSetCount": map(select(.kind == "StatefulSet")) | length
}' "${TEMPORARY_DIRECTORY}/default.yaml" >"${TEMPORARY_DIRECTORY}/manifest-counts.yaml"
assert_yq \
    '.statefulSetCount == 1' \
    "${TEMPORARY_DIRECTORY}/manifest-counts.yaml" \
    'defaults must render exactly one StatefulSet'

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
    'ReadWriteMany persistence access mode' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" \
        --set persistence.accessModes[0]=ReadWriteMany
expect_failure \
    'insufficient terminationGracePeriodSeconds' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" \
        --set terminationGracePeriodSeconds=35
expect_failure \
    'empty resources.requests.cpu' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" --set resources.requests.cpu=
expect_failure \
    'empty resources.limits.memory' \
    helm template github-webhook-exporter "${CHART_DIRECTORY}" --set resources.limits.memory=
