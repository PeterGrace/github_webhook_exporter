#!/usr/bin/env bash
set -Eeuo pipefail

readonly VERSION="${1:-}"
readonly LOCAL_IMAGE="${2:-}"
readonly CHART_ARCHIVE="${3:-}"
readonly IMAGE_REFERENCE="ghcr.io/petergrace/github-webhook-exporter:${VERSION}"
readonly CHART_REFERENCE="oci://ghcr.io/petergrace/charts/github-webhook-exporter"
readonly CHART_REFERENCE_PATH="${CHART_REFERENCE#oci://}"
readonly CHART_REPOSITORY="oci://ghcr.io/petergrace/charts"
TEMPORARY_DIRECTORY=""
LOCAL_IMAGE_ID=""
REMOTE_IMAGE_STATE=""
CHART_STATE=""

fail() {
    printf 'release publication failed: %s\n' "$1" >&2
    exit 1
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
    [[ $# -eq 3 ]] || fail "expected VERSION LOCAL_IMAGE CHART_ARCHIVE"
    [[ "${VERSION}" =~ ^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$ ]] \
        || fail "version must be a canonical semantic version"
    [[ "${LOCAL_IMAGE}" == "${IMAGE_REFERENCE}" ]] \
        || fail "local image must match ${IMAGE_REFERENCE}"
    [[ -f "${CHART_ARCHIVE}" ]] || fail "chart archive is missing"

    local expected_archive_name="github-webhook-exporter-${VERSION}.tgz"
    [[ "${CHART_ARCHIVE##*/}" == "${expected_archive_name}" ]] \
        || fail "chart archive must be named ${expected_archive_name}"

    require_command docker
    require_command helm
    require_command mktemp
    require_command python3
    require_command rm
}

create_temporary_directory() {
    if ! TEMPORARY_DIRECTORY="$(mktemp -d)"; then
        fail "could not create private temporary directory"
    fi
}

capture_local_image_id() {
    local stdout_path="${TEMPORARY_DIRECTORY}/stdout"
    local stderr_path="${TEMPORARY_DIRECTORY}/stderr"

    if ! docker image inspect --format '{{.Id}}' "${LOCAL_IMAGE}" \
        >"${stdout_path}" 2>"${stderr_path}"; then
        fail "local image inspection failed"
    fi

    LOCAL_IMAGE_ID="$(<"${stdout_path}")"
    [[ "${LOCAL_IMAGE_ID}" =~ ^sha256:[0-9a-f]{64}$ ]] \
        || fail "local image inspection failed"
}

parse_remote_image_state() {
    local local_image_id="$1"
    local manifest_json="$2"
    local python_output

    if ! python_output="$(python3 -c '
import json
import re
import sys

local_image_id = sys.argv[1]
manifest = json.loads(sys.argv[2])
if not isinstance(manifest, dict):
    raise SystemExit(1)
if "manifests" in manifest:
    print("index")
    raise SystemExit(0)
config = manifest.get("config")
if not isinstance(config, dict):
    raise SystemExit(1)
config_digest = config.get("digest")
if not isinstance(config_digest, str):
    raise SystemExit(1)
if re.fullmatch(r"sha256:[0-9a-f]{64}", config_digest) is None:
    raise SystemExit(1)
# For a single-platform manifest, both values identify the same image config blob.
print("matching" if config_digest == local_image_id else "different")
' "${local_image_id}" "${manifest_json}" 2>"${TEMPORARY_DIRECTORY}/parser.stderr")"; then
        fail "image inspection failed"
    fi

    REMOTE_IMAGE_STATE="${python_output}"
}

inspect_remote_image_state() {
    local stdout_path="${TEMPORARY_DIRECTORY}/stdout"
    local stderr_path="${TEMPORARY_DIRECTORY}/stderr"
    local diagnostics manifest_json

    if ! docker manifest inspect "${IMAGE_REFERENCE}" >"${stdout_path}" 2>"${stderr_path}"; then
        diagnostics="$(<"${stderr_path}")"
        if [[ "${diagnostics}" == *"manifest unknown"* \
            || "${diagnostics}" == *"no such manifest"* ]]; then
            REMOTE_IMAGE_STATE="missing"
            return 0
        fi
        fail "image inspection failed"
    fi

    manifest_json="$(<"${stdout_path}")"
    parse_remote_image_state "${LOCAL_IMAGE_ID}" "${manifest_json}"
}

inspect_chart_state() {
    local stdout_path stderr_path diagnostics chart_not_found_marker
    stdout_path="${TEMPORARY_DIRECTORY}/stdout"
    stderr_path="${TEMPORARY_DIRECTORY}/stderr"
    chart_not_found_marker="${CHART_REFERENCE_PATH}:${VERSION}: not found"

    if ! helm show chart "${CHART_REFERENCE}" --version "${VERSION}" \
        >"${stdout_path}" 2>"${stderr_path}"; then
        diagnostics="$(<"${stderr_path}")"
        if [[ "${diagnostics}" == *"${chart_not_found_marker}"* ]]; then
            CHART_STATE="missing"
            return 0
        fi
        fail "chart inspection failed"
    fi

    CHART_STATE="present"
}

push_image() {
    local stdout_path="${TEMPORARY_DIRECTORY}/stdout"
    local stderr_path="${TEMPORARY_DIRECTORY}/stderr"

    if ! docker push "${IMAGE_REFERENCE}" >"${stdout_path}" 2>"${stderr_path}"; then
        fail "image push failed"
    fi
}

push_chart() {
    local failure_message="$1"
    local stdout_path="${TEMPORARY_DIRECTORY}/stdout"
    local stderr_path="${TEMPORARY_DIRECTORY}/stderr"

    if ! helm push "${CHART_ARCHIVE}" "${CHART_REPOSITORY}" \
        >"${stdout_path}" 2>"${stderr_path}"; then
        fail "${failure_message}"
    fi
}

main() {
    validate_inputs "$@"
    create_temporary_directory

    capture_local_image_id
    inspect_remote_image_state

    if [[ "${REMOTE_IMAGE_STATE}" == "index" ]]; then
        fail "unsupported remote image manifest"
    fi

    if [[ "${REMOTE_IMAGE_STATE}" == "different" ]]; then
        inspect_chart_state
        case "${CHART_STATE}" in
            missing|present)
                fail "digest conflict"
                ;;
            *)
                fail "chart inspection failed"
                ;;
        esac
    fi

    inspect_chart_state

    case "${REMOTE_IMAGE_STATE}:${CHART_STATE}" in
        missing:missing)
            push_image
            push_chart \
                "chart push failed after the image was published; rerunning the same release can recover only if the rebuilt image digest matches"
            printf 'published image and chart for %s\n' "${VERSION}"
            ;;
        matching:missing)
            push_chart "chart push failed during chart-only recovery"
            printf 'completed chart-only recovery for %s\n' "${VERSION}"
            ;;
        missing:present)
            fail "chart already exists while the image is absent"
            ;;
        matching:present)
            fail "release is already published"
            ;;
        *)
            fail "unsupported publication state ${REMOTE_IMAGE_STATE}:${CHART_STATE}"
            ;;
    esac
}

main "$@"
