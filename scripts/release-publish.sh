#!/usr/bin/env bash
set -Eeuo pipefail

readonly VERSION="${1:-}"
readonly LOCAL_IMAGE="${2:-}"
readonly CHART_ARCHIVE="${3:-}"
readonly IMAGE_REFERENCE="ghcr.io/petergrace/github-webhook-exporter:${VERSION}"
readonly CHART_REFERENCE="oci://ghcr.io/petergrace/charts/github-webhook-exporter"
readonly CHART_REPOSITORY="oci://ghcr.io/petergrace/charts"

fail() {
    printf 'release publication failed: %s\n' "$1" >&2
    exit 1
}

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
    require_command python3
}

capture_local_image_id() {
    local stdout_path stderr_path local_image_id
    stdout_path="$(mktemp)"
    stderr_path="$(mktemp)"

    if ! docker image inspect --format '{{.Id}}' "${LOCAL_IMAGE}" >"${stdout_path}" 2>"${stderr_path}"; then
        rm -f -- "${stdout_path}" "${stderr_path}"
        fail "local image inspection failed"
    fi

    local_image_id="$(<"${stdout_path}")"
    rm -f -- "${stdout_path}" "${stderr_path}"

    [[ "${local_image_id}" =~ ^sha256:[0-9a-f]{64}$ ]] || fail "local image inspection failed"
    printf '%s\n' "${local_image_id}"
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
print("matching" if config_digest == local_image_id else "different")
' "${local_image_id}" "${manifest_json}")"; then
        fail "image inspection failed"
    fi

    printf '%s\n' "${python_output}"
}

inspect_remote_image_state() {
    local local_image_id="$1"
    local stdout_path stderr_path diagnostics manifest_json remote_image_state
    stdout_path="$(mktemp)"
    stderr_path="$(mktemp)"

    if ! docker manifest inspect "${IMAGE_REFERENCE}" >"${stdout_path}" 2>"${stderr_path}"; then
        diagnostics="$(<"${stderr_path}")"
        rm -f -- "${stdout_path}" "${stderr_path}"
        if [[ "${diagnostics}" == *"manifest unknown"* || "${diagnostics}" == *"no such manifest"* ]]; then
            printf 'missing\n'
            return 0
        fi
        fail "image inspection failed"
    fi

    manifest_json="$(<"${stdout_path}")"
    rm -f -- "${stdout_path}" "${stderr_path}"

    remote_image_state="$(parse_remote_image_state "${local_image_id}" "${manifest_json}")"
    printf '%s\n' "${remote_image_state}"
}

inspect_chart_state() {
    local stdout_path stderr_path diagnostics
    stdout_path="$(mktemp)"
    stderr_path="$(mktemp)"

    if ! helm show chart "${CHART_REFERENCE}" --version "${VERSION}" \
        >"${stdout_path}" 2>"${stderr_path}"; then
        diagnostics="$(<"${stderr_path}")"
        rm -f -- "${stdout_path}" "${stderr_path}"
        if [[ "${diagnostics}" == *"not found"* ]]; then
            printf 'missing\n'
            return 0
        fi
        fail "chart inspection failed"
    fi

    rm -f -- "${stdout_path}" "${stderr_path}"
    printf 'present\n'
}

push_image() {
    local stdout_path stderr_path
    stdout_path="$(mktemp)"
    stderr_path="$(mktemp)"

    if ! docker push "${IMAGE_REFERENCE}" >"${stdout_path}" 2>"${stderr_path}"; then
        rm -f -- "${stdout_path}" "${stderr_path}"
        fail "image push failed"
    fi

    rm -f -- "${stdout_path}" "${stderr_path}"
}

push_chart() {
    local failure_message="$1"
    local stdout_path stderr_path
    stdout_path="$(mktemp)"
    stderr_path="$(mktemp)"

    if ! helm push "${CHART_ARCHIVE}" "${CHART_REPOSITORY}" >"${stdout_path}" 2>"${stderr_path}"; then
        rm -f -- "${stdout_path}" "${stderr_path}"
        fail "${failure_message}"
    fi

    rm -f -- "${stdout_path}" "${stderr_path}"
}

main() {
    validate_inputs "$@"

    local local_image_id remote_image_state chart_state
    local_image_id="$(capture_local_image_id)"
    remote_image_state="$(inspect_remote_image_state "${local_image_id}")"

    if [[ "${remote_image_state}" == "index" ]]; then
        fail "unsupported remote image manifest"
    fi

    if [[ "${remote_image_state}" == "different" ]]; then
        chart_state="$(inspect_chart_state)"
        case "${chart_state}" in
            missing|present)
                fail "digest conflict"
                ;;
            *)
                fail "chart inspection failed"
                ;;
        esac
    fi

    chart_state="$(inspect_chart_state)"

    case "${remote_image_state}:${chart_state}" in
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
            fail "unsupported publication state ${remote_image_state}:${chart_state}"
            ;;
    esac
}

main "$@"
