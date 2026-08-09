#!/usr/bin/env bash
set -Eeuo pipefail

readonly RELEASE_TAG="${1:-}"
readonly CARGO_MANIFEST="${2:-Cargo.toml}"
readonly CHART_METADATA="${3:-charts/github-webhook-exporter/Chart.yaml}"

fail() {
    printf 'release version validation failed: %s\n' "$1" >&2
    exit 1
}

extract_cargo_version() {
    awk '
        /^\[package\][[:space:]]*$/ { in_package = 1; next }
        /^\[/ { in_package = 0 }
        in_package && /^[[:space:]]*version[[:space:]]*=/ {
            value = $0
            sub(/^[^=]*=[[:space:]]*/, "", value)
            gsub(/^"|"[[:space:]]*$/, "", value)
            print value
            exit
        }
    ' "$1"
}

extract_chart_value() {
    local key="$1"
    awk -v key="${key}" '
        $0 ~ ("^" key ":[[:space:]]*") {
            value = $0
            sub("^" key ":[[:space:]]*", "", value)
            gsub(/^"|"[[:space:]]*$/, "", value)
            print value
            exit
        }
    ' "$2"
}

[[ -f "${CARGO_MANIFEST}" ]] || fail "Cargo manifest is missing"
[[ -f "${CHART_METADATA}" ]] || fail "Helm chart metadata is missing"

if [[ ! "${RELEASE_TAG}" =~ ^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]]; then
    fail "tag must be a stable semantic-version tag in vMAJOR.MINOR.PATCH form"
fi

readonly RELEASE_VERSION="${RELEASE_TAG#v}"
readonly CARGO_VERSION="$(extract_cargo_version "${CARGO_MANIFEST}")"
readonly CHART_VERSION="$(extract_chart_value version "${CHART_METADATA}")"
readonly APP_VERSION="$(extract_chart_value appVersion "${CHART_METADATA}")"

[[ -n "${CARGO_VERSION}" ]] || fail "Cargo package version is missing"
[[ -n "${CHART_VERSION}" ]] || fail "Helm chart version is missing"
[[ -n "${APP_VERSION}" ]] || fail "Helm appVersion is missing"
[[ "${CARGO_VERSION}" == "${RELEASE_VERSION}" ]] \
    || fail "Cargo package version does not match the release tag"
[[ "${CHART_VERSION}" == "${RELEASE_VERSION}" ]] \
    || fail "Helm chart version does not match the release tag"
[[ "${APP_VERSION}" == "${RELEASE_VERSION}" ]] \
    || fail "Helm appVersion does not match the release tag"

printf '%s\n' "${RELEASE_VERSION}"
