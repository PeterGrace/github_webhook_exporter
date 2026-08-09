#!/usr/bin/env bash
set -Eeuo pipefail

readonly CHART_DIRECTORY="${1:-}"
readonly OUTPUT_DIRECTORY="${2:-}"
SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY
readonly REPOSITORY_ROOT="${SCRIPT_DIRECTORY}/.."
readonly OUTPUT_GUARD="${SCRIPT_DIRECTORY}/helm-output-directory.sh"
readonly RENDER_SCRIPT="${SCRIPT_DIRECTORY}/helm-render-matrix.sh"
readonly KUBECONFORM_SCRIPT="${SCRIPT_DIRECTORY}/helm-kubeconform.sh"
readonly POLICY_SCRIPT="${SCRIPT_DIRECTORY}/helm-policy-test.sh"
readonly SECRET_SCAN_SCRIPT="${SCRIPT_DIRECTORY}/helm-secret-scan.sh"
readonly ARCHIVE_PREFLIGHT="${SCRIPT_DIRECTORY}/helm-archive-preflight.py"
readonly ARCHIVE_PREFLIGHT_TEST="${SCRIPT_DIRECTORY}/helm-archive-preflight-test.py"
readonly RENDER_COMPARE="${SCRIPT_DIRECTORY}/helm-render-compare.py"
readonly RENDER_CASES_FILE="${SCRIPT_DIRECTORY}/../ci/helm/render-cases.txt"
readonly KUBE_VERSION="1.31.0"
TEMPORARY_DIRECTORY=""
export HELM_OUTPUT_ERROR_PREFIX="Helm package test"

fail() {
    printf 'Helm package test failed: %s\n' "$1" >&2
    exit 1
}

usage() {
    printf 'usage: %s CHART_DIRECTORY OUTPUT_DIRECTORY\n' "${0##*/}" >&2
    exit 2
}

require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        printf 'required command not found: %s\n' "$1" >&2
        exit 2
    fi
}

cleanup() {
    if [[ -n "${TEMPORARY_DIRECTORY}" ]]; then
        rm -rf -- "${TEMPORARY_DIRECTORY}"
    fi
    helm_output_cleanup_stage
}

validate_rendered_directory() {
    local rendered_directory="$1"
    "${KUBECONFORM_SCRIPT}" "${rendered_directory}" >/dev/null
    "${POLICY_SCRIPT}" "${rendered_directory}" >/dev/null
}

main() {
    if [[ -z "${CHART_DIRECTORY}" || -z "${OUTPUT_DIRECTORY}" ]]; then
        usage
    fi
    for command in find helm mktemp mv python3 rm rmdir sort; do
        require_command "${command}"
    done
    for required_file in "${OUTPUT_GUARD}" "${ARCHIVE_PREFLIGHT}" \
        "${ARCHIVE_PREFLIGHT_TEST}" "${RENDER_COMPARE}" "${RENDER_CASES_FILE}"; do
        if [[ ! -f "${required_file}" ]]; then
            fail "missing package validation component"
        fi
    done

    # shellcheck source=helm-output-directory.sh disable=SC1091
    source "${OUTPUT_GUARD}"
    helm_output_prepare "helm-package" "${CHART_DIRECTORY}" "${OUTPUT_DIRECTORY}" \
        "${REPOSITORY_ROOT}"
    trap cleanup EXIT
    TEMPORARY_DIRECTORY="$(mktemp -d)"

    python3 "${ARCHIVE_PREFLIGHT_TEST}" >/dev/null

    local source_rendered_directory="${TEMPORARY_DIRECTORY}/source-rendered"
    local archive_rendered_directory="${TEMPORARY_DIRECTORY}/archive-rendered"
    "${RENDER_SCRIPT}" "${CHART_DIRECTORY}" "${source_rendered_directory}" >/dev/null

    helm package "${CHART_DIRECTORY}" --destination "${HELM_OUTPUT_STAGE}" >/dev/null
    local -a package_files=()
    mapfile -t package_files < <(
        find "${HELM_OUTPUT_STAGE}" -maxdepth 1 -type f -name '*.tgz' | sort
    )
    if [[ ${#package_files[@]} -ne 1 ]]; then
        fail "expected exactly one packaged archive, found ${#package_files[@]}"
    fi

    local package_path="${package_files[0]}"
    local chart_metadata chart_name chart_version expected_package_name
    chart_metadata="$(helm show chart "${package_path}")"
    chart_name="$(awk '/^name:[[:space:]]*/ { sub(/^name:[[:space:]]*/, ""); print; exit }' \
        <<<"${chart_metadata}")"
    chart_version="$(awk '/^version:[[:space:]]*/ { sub(/^version:[[:space:]]*/, ""); print; exit }' \
        <<<"${chart_metadata}")"
    [[ -n "${chart_name}" && -n "${chart_version}" ]] \
        || fail "packaged archive metadata is missing name or version"
    expected_package_name="${chart_name}-${chart_version}.tgz"
    if [[ "${package_path##*/}" != "${expected_package_name}" ]]; then
        fail "unexpected packaged archive name"
    fi
    helm show values "${package_path}" >/dev/null
    helm template archive "${package_path}" --kube-version "${KUBE_VERSION}" >/dev/null

    local archive_root="${CHART_DIRECTORY%/}"
    archive_root="${archive_root##*/}"
    python3 "${ARCHIVE_PREFLIGHT}" "${package_path}" "${archive_root}" \
        "${TEMPORARY_DIRECTORY}"
    local extracted_chart_directory="${TEMPORARY_DIRECTORY}/${archive_root}"
    if [[ ! -d "${extracted_chart_directory}" ]]; then
        fail "missing extracted chart directory"
    fi

    "${RENDER_SCRIPT}" "${extracted_chart_directory}" "${archive_rendered_directory}" >/dev/null
    python3 "${RENDER_COMPARE}" "${source_rendered_directory}" \
        "${archive_rendered_directory}" "${RENDER_CASES_FILE}"

    validate_rendered_directory "${source_rendered_directory}"
    validate_rendered_directory "${archive_rendered_directory}"
    "${SECRET_SCAN_SCRIPT}" --test "${CHART_DIRECTORY}" "${source_rendered_directory}" >/dev/null
    "${SECRET_SCAN_SCRIPT}" "${extracted_chart_directory}" \
        "${archive_rendered_directory}" >/dev/null

    helm_output_commit
    printf 'Validated packaged Helm chart: %s/%s\n' "${HELM_OUTPUT_DIRECTORY}" "${expected_package_name}"
}

main "$@"
