#!/usr/bin/env bash
set -Eeuo pipefail

REPOSITORY_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPOSITORY_ROOT
readonly VALIDATOR="${REPOSITORY_ROOT}/scripts/release-version.sh"
TEMPORARY_DIRECTORY="$(mktemp -d)"
readonly TEMPORARY_DIRECTORY
trap 'rm -rf "${TEMPORARY_DIRECTORY}"' EXIT

fail() {
    printf 'release version test failed: %s\n' "$1" >&2
    exit 1
}

write_metadata() {
    local cargo_version="$1"
    local chart_version="$2"
    local app_version="$3"

    cat >"${TEMPORARY_DIRECTORY}/Cargo.toml" <<EOF
[package]
name = "fixture"
version = "${cargo_version}"
EOF
    cat >"${TEMPORARY_DIRECTORY}/Chart.yaml" <<EOF
apiVersion: v2
name: fixture
version: ${chart_version}
appVersion: "${app_version}"
EOF
}

run_success_case() {
    local tag="$1"
    local expected_version="$2"
    local output

    output="$(${VALIDATOR} "${tag}" \
        "${TEMPORARY_DIRECTORY}/Cargo.toml" \
        "${TEMPORARY_DIRECTORY}/Chart.yaml")" \
        || fail "expected ${tag} to pass"
    [[ "${output}" == "${expected_version}" ]] \
        || fail "expected normalized version ${expected_version}, received ${output}"
}

run_failure_case() {
    local tag="$1"
    local expected_diagnostic="$2"
    local output

    if output="$(${VALIDATOR} "${tag}" \
        "${TEMPORARY_DIRECTORY}/Cargo.toml" \
        "${TEMPORARY_DIRECTORY}/Chart.yaml" 2>&1)"; then
        fail "expected ${tag} to fail"
    fi
    [[ "${output}" == *"${expected_diagnostic}"* ]] \
        || fail "missing diagnostic '${expected_diagnostic}' for ${tag}: ${output}"
}

write_metadata "1.2.3" "1.2.3" "1.2.3"
run_success_case "v1.2.3" "1.2.3"

for invalid_tag in \
    "1.2.3" \
    "v1.2" \
    "v1.2.3-rc.1" \
    "v1.2.3+build.1" \
    "v01.2.3" \
    "v1.02.3" \
    "v1.2.03"; do
    run_failure_case "${invalid_tag}" "stable semantic-version tag"
done

write_metadata "1.2.4" "1.2.3" "1.2.3"
run_failure_case "v1.2.3" "Cargo package version"

write_metadata "1.2.3" "1.2.4" "1.2.3"
run_failure_case "v1.2.3" "Helm chart version"

write_metadata "1.2.3" "1.2.3" "1.2.4"
run_failure_case "v1.2.3" "Helm appVersion"

write_metadata "" "1.2.3" "1.2.3"
run_failure_case "v1.2.3" "Cargo package version is missing"

write_metadata "1.2.3" "" "1.2.3"
run_failure_case "v1.2.3" "Helm chart version is missing"

write_metadata "1.2.3" "1.2.3" ""
run_failure_case "v1.2.3" "Helm appVersion is missing"

printf 'release version tests passed\n'
