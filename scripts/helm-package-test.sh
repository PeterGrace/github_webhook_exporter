#!/usr/bin/env bash
set -Eeuo pipefail

readonly CHART_DIRECTORY="${1:-}"
readonly OUTPUT_DIRECTORY="${2:-}"
SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY
readonly RENDER_SCRIPT="${SCRIPT_DIRECTORY}/helm-render-matrix.sh"
readonly KUBECONFORM_SCRIPT="${SCRIPT_DIRECTORY}/helm-kubeconform.sh"
readonly POLICY_SCRIPT="${SCRIPT_DIRECTORY}/helm-policy-test.sh"
readonly SECRET_SCAN_SCRIPT="${SCRIPT_DIRECTORY}/helm-secret-scan.sh"
readonly PACKAGE_NAME="github-webhook-exporter-0.1.0.tgz"
readonly KUBE_VERSION="1.31.0"
TEMPORARY_DIRECTORY=""

fail() {
    printf 'Helm package test failed: %s\n' "$1" >&2
    exit 1
}

usage() {
    printf 'usage: %s CHART_DIRECTORY OUTPUT_DIRECTORY\n' "${0##*/}" >&2
    exit 2
}

require_command() {
    local command_name="$1"
    if ! command -v "${command_name}" >/dev/null 2>&1; then
        printf 'required command not found: %s\n' "${command_name}" >&2
        exit 2
    fi
}

cleanup_temporary_directory() {
    if [[ -n "${TEMPORARY_DIRECTORY}" ]]; then
        rm -rf -- "${TEMPORARY_DIRECTORY}"
    fi
}

validate_archive_path() {
    local archive_root="$1"
    local archive_entry="$2"

    case "${archive_entry}" in
        ""|/*|../*|*"/../"*|*"/.."|*"//"*)
            fail "unsafe archive path: ${archive_entry}"
            ;;
    esac

    case "${archive_entry}" in
        "${archive_root}"/*|"${archive_root}")
            ;;
        *)
            fail "unexpected archive root entry: ${archive_entry}"
            ;;
    esac

    case "${archive_entry}" in
        *"/ci/"*|*"/scripts/"*|*"/dist/"*|*"/target/"*|*"/changelog/"*|\
        *"/.superpowers/"*|*"/tests/"*|*negative*)
            fail "archive contains a forbidden generated or negative path: ${archive_entry}"
            ;;
    esac
}

validate_archive_members() {
    local archive_root="$1"
    local package_path="$2"
    local suppress_errors="${3:-0}"

    while IFS=' ' read -r mode _ _ _ _ archive_entry _ ||
        [[ -n "${mode}" ]]; do
        case "${mode}" in
            ""|total)
                continue
                ;;
        esac

        local member_type="${mode:0:1}"
        case "${member_type}" in
            -|d)
                ;;
            *)
                if (( suppress_errors )); then
                    return 1
                fi
                fail "unsupported archive member type: ${member_type}"
                ;;
        esac

        archive_entry="${archive_entry%/}"
        validate_archive_path "${archive_root}" "${archive_entry}"
    done < <(tar -tvzf "${package_path}")
}

run_malicious_link_archive_test() {
    (
        local test_directory
        local chart_directory
        local archive_path

        test_directory="$(mktemp -d)"
        trap 'rm -rf -- "${test_directory}"' EXIT

        chart_directory="${test_directory}/chart"
        mkdir -p "${chart_directory}"
        printf 'apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: malicious\n' >"${chart_directory}/Chart.yaml"
        ln -s /etc/passwd "${chart_directory}/escape-link"
        ln "${chart_directory}/Chart.yaml" "${chart_directory}/escape-hardlink"

        archive_path="${test_directory}/malicious.tgz"
        tar -czf "${archive_path}" -C "${test_directory}" chart

        if validate_archive_members "chart" "${archive_path}" 1; then
            fail "malicious link archive unexpectedly passed validation"
        fi
    )
}

main() {
    if [[ -z "${CHART_DIRECTORY}" || -z "${OUTPUT_DIRECTORY}" ]]; then
        usage
    fi

    require_command find
    require_command helm
    require_command ln
    require_command mktemp
    require_command rm
    require_command tar

    local archive_root
    archive_root="${CHART_DIRECTORY##*/}"

    rm -rf "${OUTPUT_DIRECTORY}"
    mkdir -p "${OUTPUT_DIRECTORY}"

    run_malicious_link_archive_test

    helm package "${CHART_DIRECTORY}" --destination "${OUTPUT_DIRECTORY}" >/dev/null

    local -a package_files=()
    mapfile -t package_files < <(
        find "${OUTPUT_DIRECTORY}" -maxdepth 1 -type f -name '*.tgz' | sort
    )

    if [[ ${#package_files[@]} -ne 1 ]]; then
        fail "expected exactly one packaged archive, found ${#package_files[@]}"
    fi

    local package_path
    package_path="${package_files[0]}"
    if [[ "${package_path##*/}" != "${PACKAGE_NAME}" ]]; then
        fail "unexpected packaged archive name: ${package_path##*/}"
    fi

    helm show chart "${package_path}" >/dev/null
    helm show values "${package_path}" >/dev/null
    helm template archive "${package_path}" --kube-version "${KUBE_VERSION}" >/dev/null

    TEMPORARY_DIRECTORY="$(mktemp -d)"
    trap cleanup_temporary_directory EXIT

    validate_archive_members "${archive_root}" "${package_path}"

    tar --no-same-owner --no-same-permissions -xzf "${package_path}" -C "${TEMPORARY_DIRECTORY}"

    local extracted_chart_directory="${TEMPORARY_DIRECTORY}/${archive_root}"
    if [[ ! -d "${extracted_chart_directory}" ]]; then
        fail "missing extracted chart directory: ${extracted_chart_directory}"
    fi

    local rendered_directory="${TEMPORARY_DIRECTORY}/rendered"
    "${RENDER_SCRIPT}" "${extracted_chart_directory}" "${rendered_directory}" >/dev/null
    "${KUBECONFORM_SCRIPT}" "${extracted_chart_directory}" >/dev/null
    "${POLICY_SCRIPT}" "${extracted_chart_directory}" >/dev/null
    "${SECRET_SCAN_SCRIPT}" "${extracted_chart_directory}" "${rendered_directory}" >/dev/null

    printf 'Validated packaged Helm chart: %s\n' "${package_path}"
}

main "$@"
