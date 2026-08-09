#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY
REPOSITORY_ROOT="$(cd "${SCRIPT_DIRECTORY}/.." && pwd)"
readonly REPOSITORY_ROOT
readonly TEST_TAG_PREFIX="github-webhook-exporter:reproducibility-$$"
readonly FIRST_IMAGE="${TEST_TAG_PREFIX}-first"
readonly SECOND_IMAGE="${TEST_TAG_PREFIX}-second"

cleanup() {
    docker image rm --force "${FIRST_IMAGE}" "${SECOND_IMAGE}" >/dev/null 2>&1 || true
}

terminate() {
    exit "$1"
}

fail() {
    printf 'production image reproducibility test failed: %s\n' "$1" >&2
    exit 1
}

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

build_image() {
    local image_reference="$1"
    local source_date_epoch="$2"
    local created_timestamp="$3"

    docker build \
        --no-cache \
        --platform linux/amd64 \
        --provenance=false \
        --build-arg "SOURCE_DATE_EPOCH=${source_date_epoch}" \
        --label "org.opencontainers.image.created=${created_timestamp}" \
        --tag "${image_reference}" \
        "${REPOSITORY_ROOT}"
}

main() {
    require_command docker
    require_command git

    trap cleanup EXIT
    trap 'terminate 129' HUP
    trap 'terminate 130' INT
    trap 'terminate 143' TERM

    local source_date_epoch created_timestamp first_image_id second_image_id
    source_date_epoch="$(git -C "${REPOSITORY_ROOT}" show -s --format=%ct HEAD)"
    created_timestamp="$(git -C "${REPOSITORY_ROOT}" show -s --format=%cI HEAD)"
    [[ "${source_date_epoch}" =~ ^[0-9]+$ ]] || fail "commit timestamp is not a Unix epoch"
    [[ -n "${created_timestamp}" ]] || fail "commit creation timestamp is empty"

    build_image "${FIRST_IMAGE}" "${source_date_epoch}" "${created_timestamp}"
    first_image_id="$(docker image inspect --format '{{.Id}}' "${FIRST_IMAGE}")"
    printf 'first image ID: %s\n' "${first_image_id}"

    build_image "${SECOND_IMAGE}" "${source_date_epoch}" "${created_timestamp}"
    second_image_id="$(docker image inspect --format '{{.Id}}' "${SECOND_IMAGE}")"
    printf 'second image ID: %s\n' "${second_image_id}"

    [[ "${first_image_id}" == "${second_image_id}" ]] \
        || fail "cache-disabled builds produced different image IDs"

    printf 'production image reproducibility test passed\n'
}

main "$@"
