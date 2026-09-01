#!/usr/bin/env bash
set -Eeuo pipefail

readonly VERSION="${1:-}"
readonly CHART_ARCHIVE="${2:-}"
readonly RELEASE_TAG="v${VERSION}"
SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY
readonly CHANGELOG_SCRIPT="${SCRIPT_DIRECTORY}/release-changelog.sh"
TEMPORARY_DIRECTORY=""

fail() {
    printf 'release announcement failed: %s\n' "$1" >&2
    exit 1
}

usage() {
    printf 'usage: %s VERSION CHART_ARCHIVE\n' "${0##*/}" >&2
    exit 2
}

cleanup() {
    if [[ -n "${TEMPORARY_DIRECTORY}" ]]; then
        rm -rf -- "${TEMPORARY_DIRECTORY}"
        TEMPORARY_DIRECTORY=""
    fi
}

terminate() {
    local exit_status="$1"
    trap - EXIT HUP INT TERM
    cleanup
    exit "${exit_status}"
}

trap cleanup EXIT
trap 'terminate 129' HUP
trap 'terminate 130' INT
trap 'terminate 143' TERM

require_command() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}

validate_inputs() {
    [[ $# -eq 2 ]] || usage
    [[ "${VERSION}" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
        || fail "version must be a canonical semantic version"
    [[ -f "${CHART_ARCHIVE}" ]] || fail "chart archive is missing"

    local expected_archive_name="github-webhook-exporter-${VERSION}.tgz"
    [[ "${CHART_ARCHIVE##*/}" == "${expected_archive_name}" ]] \
        || fail "chart archive must be named ${expected_archive_name}"

    [[ -x "${CHANGELOG_SCRIPT}" ]] || fail "changelog generator is missing"

    require_command gh
    require_command mktemp
    require_command rm
}

create_temporary_directory() {
    if ! TEMPORARY_DIRECTORY="$(mktemp -d)"; then
        fail "could not create private temporary directory"
    fi
}

# A release page is a description of an already-published artifact set, so an existing page is
# never rewritten. Chart-only recovery reruns therefore stay idempotent instead of failing closed.
release_exists() {
    local stdout_path="${TEMPORARY_DIRECTORY}/stdout"
    local stderr_path="${TEMPORARY_DIRECTORY}/stderr"
    local diagnostics

    if gh release view "${RELEASE_TAG}" >"${stdout_path}" 2>"${stderr_path}"; then
        return 0
    fi

    diagnostics="$(<"${stderr_path}")"
    if [[ "${diagnostics}" == *"release not found"* ]]; then
        return 1
    fi
    fail "release inspection failed"
}

generate_notes() {
    local notes_path="${TEMPORARY_DIRECTORY}/notes.md"
    local stderr_path="${TEMPORARY_DIRECTORY}/stderr"

    if ! "${CHANGELOG_SCRIPT}" "${VERSION}" >"${notes_path}" 2>"${stderr_path}"; then
        fail "changelog generation failed"
    fi
    [[ -s "${notes_path}" ]] || fail "changelog generation produced no notes"
    printf '%s\n' "${notes_path}"
}

create_release() {
    local notes_path="$1"
    local stdout_path="${TEMPORARY_DIRECTORY}/stdout"
    local stderr_path="${TEMPORARY_DIRECTORY}/stderr"

    # --verify-tag refuses to invent a tag: the release can only describe the tag CI validated.
    if ! gh release create "${RELEASE_TAG}" \
        --title "${RELEASE_TAG}" \
        --notes-file "${notes_path}" \
        --verify-tag \
        "${CHART_ARCHIVE}" \
        >"${stdout_path}" 2>"${stderr_path}"; then
        fail "release creation failed"
    fi
}

main() {
    validate_inputs "$@"
    create_temporary_directory

    if release_exists; then
        printf 'release notes already published for %s\n' "${RELEASE_TAG}"
        return 0
    fi

    local notes_path
    notes_path="$(generate_notes)"
    create_release "${notes_path}"
    printf 'published release notes for %s\n' "${RELEASE_TAG}"
}

main "$@"
