#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY
readonly CHART_VALUES_DIRECTORY="${SCRIPT_DIRECTORY}/../ci/helm/values"
readonly NEGATIVE_FIXTURE_DIRECTORY="${SCRIPT_DIRECTORY}/../ci/helm/negative/secrets"
readonly NEGATIVE_CASES_FILE="${SCRIPT_DIRECTORY}/../ci/helm/secret-negative-cases.txt"
readonly RENDER_SCRIPT="${SCRIPT_DIRECTORY}/helm-render-matrix.sh"
RENDERED_DIRECTORY=""

fail() {
    printf 'Helm secret scan failed: %s\n' "$1" >&2
    exit 1
}

usage() {
    printf 'usage: %s [--test CHART_DIRECTORY] PATH...\n' "${0##*/}" >&2
    exit 2
}

cleanup_rendered_directory() {
    if [[ -n "${RENDERED_DIRECTORY}" ]]; then
        rm -rf -- "${RENDERED_DIRECTORY}"
    fi
}

require_command() {
    local command_name="$1"
    if ! command -v "${command_name}" >/dev/null 2>&1; then
        printf 'required command not found: %s\n' "${command_name}" >&2
        exit 2
    fi
}

is_scannable_file() {
    local file_path="$1"
    case "${file_path}" in
        *.yaml|*.yml|*.json|*.txt|*.tpl|*.md)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

is_excluded_path() {
    local file_path="$1"
    case "${file_path}" in
        */ci/helm/negative/secrets/*|*/ci/helm/secret-negative-cases.txt|*/ci/helm/*negative-cases.txt)
            return 0
            ;;
        *)
            return 1
            ;;
    esac
}

scan_file() {
    local file_path="$1"
    local line_number=0
    local matched=0
    local inside_secret_key_ref=0

    if ! grep -Iq . "${file_path}"; then
        return 0
    fi
    local secret_key_ref_indent=0
    local -a lines=()
    mapfile -t lines <"${file_path}"

    for line in "${lines[@]}"; do
        line_number=$((line_number + 1))

        if [[ "${line}" =~ ^([[:space:]]*)secretKeyRef:[[:space:]]*$ ]]; then
            inside_secret_key_ref=1
            secret_key_ref_indent=${#BASH_REMATCH[1]}
            continue
        fi

        if (( inside_secret_key_ref )); then
            if [[ -n "${line}" ]] && [[ "${line}" =~ ^([[:space:]]*) ]]; then
                local current_indent=${#BASH_REMATCH[1]}
                if (( current_indent <= secret_key_ref_indent )); then
                    inside_secret_key_ref=0
                else
                    continue
                fi
            fi
        fi

        if (( inside_secret_key_ref )); then
            continue
        fi

        if [[ "${file_path}" == *values.yaml ]]; then
            if [[ "${line}" =~ ^[[:space:]]*masterKey:[[:space:]]*master-key[[:space:]]*$ ||
                  "${line}" =~ ^[[:space:]]*adminToken:[[:space:]]*admin-token[[:space:]]*$ ]]; then
                continue
            fi
        fi

        local category_id=""
        if [[ "${line}" =~ ^[[:space:]]*kind:[[:space:]]*\"?Secret\"?([[:space:]]*(#.*)?)?$ ]]; then
            category_id="SECRET006"
        elif [[ "${line}" =~ ^[[:space:]]*OTEL_EXPORTER_OTLP(_TRACES|_LOGS)?_HEADERS[[:space:]]*[:=][[:space:]]*.+$ ]]; then
            category_id="SECRET005"
        elif [[ "${line}" =~ ^[[:space:]]*[Aa]uthorization[[:space:]]*[:=][[:space:]]*.+$ ]]; then
            category_id="SECRET004"
        elif [[ "${line}" =~ [:=][[:space:]]*\"?webhook-secret\"?([[:space:]]*,)?([[:space:]]*(#.*)?)?$ ]]; then
            category_id="SECRET003"
        elif [[ "${line}" =~ [:=][[:space:]]*\"?master-key\"?([[:space:]]*,)?([[:space:]]*(#.*)?)?$ ]]; then
            category_id="SECRET002"
        elif [[ "${line}" =~ [:=][[:space:]]*\"?fixture-token\"?([[:space:]]*,)?([[:space:]]*(#.*)?)?$ ]]; then
            category_id="SECRET001"
        fi

        if [[ -n "${category_id}" ]]; then
            printf '%s:%d:%s\n' "${file_path}" "${line_number}" "${category_id}" >&2
            matched=1
        fi
    done

    return "${matched}"
}

collect_files() {
    local exclude_negative_fixtures="$1"
    shift

    local root_path
    for root_path in "$@"; do
        if [[ ! -e "${root_path}" ]]; then
            fail "path not found: ${root_path}"
        fi

        if [[ -f "${root_path}" ]]; then
            if is_scannable_file "${root_path}"; then
                if (( exclude_negative_fixtures )) && is_excluded_path "${root_path}"; then
                    continue
                fi
                printf '%s\n' "${root_path}"
            fi
            continue
        fi

        if [[ -d "${root_path}" ]]; then
            if (( exclude_negative_fixtures )); then
                find "${root_path}" \
                    \( -path '*/ci/helm/negative/secrets' -o \
                       -path '*/ci/helm/negative/secrets/*' -o \
                       -path '*/ci/helm/secret-negative-cases.txt' -o \
                       -path '*/ci/helm/*negative-cases.txt' \) -prune -o \
                    -type f \( -name '*.yaml' -o -name '*.yml' -o -name '*.json' -o \
                               -name '*.txt' -o -name '*.tpl' -o -name '*.md' \) -print
            else
                find "${root_path}" -type f \( -name '*.yaml' -o -name '*.yml' -o -name '*.json' -o \
                                                -name '*.txt' -o -name '*.tpl' -o -name '*.md' \) -print
            fi
            continue
        fi

        fail "unsupported path type: ${root_path}"
    done | sort
}

scan_paths() {
    local exclude_negative_fixtures="$1"
    shift

    local overall_status=0
    local scan_path
    while IFS= read -r scan_path; do
        if ! scan_file "${scan_path}"; then
            overall_status=1
        fi
    done < <(collect_files "${exclude_negative_fixtures}" "$@")

    return "${overall_status}"
}

run_positive_checks() {
    local chart_directory="$1"
    RENDERED_DIRECTORY="$(mktemp -d)"
    trap cleanup_rendered_directory EXIT

    "${RENDER_SCRIPT}" "${chart_directory}" "${RENDERED_DIRECTORY}" >/dev/null

    scan_paths 1 "${chart_directory}" || {
        fail "chart source scan failed"
    }
    scan_paths 1 "${CHART_VALUES_DIRECTORY}" || {
        fail "values fixture scan failed"
    }
    scan_paths 1 "${RENDERED_DIRECTORY}" || {
        fail "rendered manifest scan failed"
    }
}

run_negative_checks() {
    local case_file
    case_file="${NEGATIVE_CASES_FILE}"
    if [[ ! -f "${case_file}" ]]; then
        fail "missing negative case contract: ${case_file}"
    fi

    while IFS= read -r case_line || [[ -n "${case_line}" ]]; do
        case_line="${case_line%$'\r'}"
        [[ -z "${case_line}" || "${case_line}" == \#* ]] && continue

        local expected_category fixture_name
        read -r expected_category fixture_name <<<"${case_line}"
        if [[ -z "${expected_category:-}" || -z "${fixture_name:-}" ]]; then
            fail "invalid negative case contract line: ${case_line}"
        fi

        local fixture_path="${NEGATIVE_FIXTURE_DIRECTORY}/${fixture_name}"
        local fixture_output
        if fixture_output="$(scan_paths 0 "${fixture_path}" 2>&1)"; then
            fail "negative fixture unexpectedly passed: ${fixture_name}"
        fi

        local actual_categories
        actual_categories="$(grep -oE 'SECRET[0-9]{3}' <<<"${fixture_output}" | sort -u | tr '\n' ' ')"
        actual_categories="${actual_categories% }"
        if [[ "${actual_categories}" != "${expected_category}" ]]; then
            fail "negative fixture ${fixture_name} reported ${actual_categories:-no stable category} instead of ${expected_category}"
        fi

        printf 'Rejected %s with %s\n' "${fixture_name}" "${expected_category}" >&2
    done <"${case_file}"
}

main() {
    require_command find
    require_command grep
    require_command mktemp
    require_command rm
    require_command sort
    require_command tr

    if [[ ${1:-} == --test ]]; then
        shift
        if [[ $# -ne 1 ]]; then
            usage
        fi
        run_positive_checks "$1"
        run_negative_checks
        return 0
    fi

    if [[ $# -lt 1 ]]; then
        usage
    fi

    scan_paths 1 "$@"
}

main "$@"
