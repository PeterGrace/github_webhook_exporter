#!/usr/bin/env bash
set -Eeuo pipefail

readonly CHART_DIRECTORY="${1:-}"
script_directory="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY="${script_directory}"
readonly RENDER_SCRIPT="${SCRIPT_DIRECTORY}/helm-render-matrix.sh"
readonly POLICY_DIRECTORY="${SCRIPT_DIRECTORY}/../ci/helm/policy"
readonly NEGATIVE_DIRECTORY="${SCRIPT_DIRECTORY}/../ci/helm/negative/policy"
readonly NEGATIVE_CASES_FILE="${SCRIPT_DIRECTORY}/../ci/helm/policy-negative-cases.txt"

fail() {
    printf 'Helm policy test failed: %s\n' "$1" >&2
    exit 1
}

if [[ -z "${CHART_DIRECTORY}" ]]; then
    printf 'usage: %s CHART_DIRECTORY\n' "${0##*/}" >&2
    exit 2
fi

for command in conftest grep mktemp rm sort tr; do
    if ! command -v "${command}" >/dev/null 2>&1; then
        printf 'required command not found: %s\n' "${command}" >&2
        exit 2
    fi
done

if [[ ! -x "${RENDER_SCRIPT}" ]]; then
    fail "missing Helm render matrix script: ${RENDER_SCRIPT}"
fi

if [[ ! -d "${POLICY_DIRECTORY}" ]]; then
    fail "missing policy directory: ${POLICY_DIRECTORY}"
fi

if [[ ! -d "${NEGATIVE_DIRECTORY}" ]]; then
    fail "missing negative policy fixture directory: ${NEGATIVE_DIRECTORY}"
fi

if [[ ! -f "${NEGATIVE_CASES_FILE}" ]]; then
    fail "missing negative cases contract: ${NEGATIVE_CASES_FILE}"
fi

temporary_directory="$(mktemp -d)"
chmod 755 "${temporary_directory}"
readonly TEMPORARY_DIRECTORY="${temporary_directory}"
cleanup() {
    rm -rf "${TEMPORARY_DIRECTORY}"
}
trap cleanup EXIT

readonly RENDER_DIRECTORY="${TEMPORARY_DIRECTORY}/rendered"
mkdir -p "${RENDER_DIRECTORY}"

"${RENDER_SCRIPT}" "${CHART_DIRECTORY}" "${RENDER_DIRECTORY}" >/dev/null

if ! supported_output="$({ conftest test --policy "${POLICY_DIRECTORY}" "${RENDER_DIRECTORY}"; } 2>&1)"; then
    fail "supported render matrix violated policy: ${supported_output}"
fi

printf 'Supported render matrix passed workload policy checks\n'

while IFS= read -r case_line || [[ -n "${case_line}" ]]; do
    case_line="${case_line%$'\r'}"
    [[ -z "${case_line}" || "${case_line}" == \#* ]] && continue

    read -r expected_rule_id fixture_name <<<"${case_line}"
    if [[ -z "${expected_rule_id:-}" || -z "${fixture_name:-}" ]]; then
        fail "invalid negative case contract line: ${case_line}"
    fi

    FIXTURE_PATH="${NEGATIVE_DIRECTORY}/${fixture_name}"
    if [[ ! -f "${FIXTURE_PATH}" ]]; then
        fail "missing negative fixture: ${fixture_name}"
    fi

    if fixture_output="$({ conftest test --policy "${POLICY_DIRECTORY}" "${FIXTURE_PATH}"; } 2>&1)"; then
        fail "negative fixture unexpectedly passed: ${fixture_name}"
    fi

    actual_rule_ids="$(grep -oE 'GWE[0-9]{3}' <<<"${fixture_output}" | sort -u | tr '\n' ' ')"
    actual_rule_ids="${actual_rule_ids% }"
    if [[ "${actual_rule_ids}" != "${expected_rule_id}" ]]; then
        fail "negative fixture ${fixture_name} reported ${actual_rule_ids:-no stable policy ID} instead of ${expected_rule_id}"
    fi

    printf 'Rejected %s with %s\n' "${fixture_name}" "${expected_rule_id}"
done <"${NEGATIVE_CASES_FILE}"
