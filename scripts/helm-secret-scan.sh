#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY
readonly STRUCTURAL_SCANNER="${SCRIPT_DIRECTORY}/helm-secret-scan.py"
readonly CHART_VALUES_DIRECTORY="${SCRIPT_DIRECTORY}/../ci/helm/values"
readonly NEGATIVE_FIXTURE_DIRECTORY="${SCRIPT_DIRECTORY}/../ci/helm/negative/secrets"
readonly NEGATIVE_CASES_FILE="${SCRIPT_DIRECTORY}/../ci/helm/secret-negative-cases.txt"

fail() {
    printf 'Helm secret scan failed: %s\n' "$1" >&2
    exit 1
}

usage() {
    printf 'usage: %s [--test CHART_DIRECTORY RENDERED_DIRECTORY] PATH...\n' "${0##*/}" >&2
    exit 2
}

require_command() {
    if ! command -v "$1" >/dev/null 2>&1; then
        printf 'required command not found: %s\n' "$1" >&2
        exit 2
    fi
}

is_scannable_file() {
    case "$1" in
        *.yaml|*.yml|*.json|*.txt|*.tpl|*.md) return 0 ;;
        *) return 1 ;;
    esac
}

is_excluded_path() {
    case "$1" in
        */ci/helm/negative/secrets/*|*/ci/helm/secret-negative-cases.txt|*/ci/helm/*negative-cases.txt)
            return 0
            ;;
        *) return 1 ;;
    esac
}

collect_files() {
    local exclude_negative_fixtures="$1"
    local result_name="$2"
    shift 2
    local -n collected_files="${result_name}"

    local root_path file_path
    for root_path in "$@"; do
        if [[ ! -e "${root_path}" ]]; then
            fail "path not found"
        elif [[ -f "${root_path}" ]]; then
            if is_scannable_file "${root_path}" &&
               { ! (( exclude_negative_fixtures )) || ! is_excluded_path "${root_path}"; }; then
                collected_files+=("${root_path}")
            fi
        elif [[ -d "${root_path}" ]]; then
            while IFS= read -r -d '' file_path; do
                if (( exclude_negative_fixtures )) && is_excluded_path "${file_path}"; then
                    continue
                fi
                collected_files+=("${file_path}")
            done < <(find "${root_path}" -type f \
                \( -name '*.yaml' -o -name '*.yml' -o -name '*.json' -o \
                   -name '*.txt' -o -name '*.tpl' -o -name '*.md' \) -print0)
        else
            fail "unsupported path type"
        fi
    done
}

scan_paths() {
    local exclude_negative_fixtures="$1"
    shift

    local -a files=()
    collect_files "${exclude_negative_fixtures}" files "$@"
    if (( ${#files[@]} == 0 )); then
        return 0
    fi
    python3 "${STRUCTURAL_SCANNER}" "${files[@]}"
}

run_positive_checks() {
    local chart_directory="$1"
    local rendered_directory="$2"

    scan_paths 1 "${chart_directory}" || fail "chart source scan failed"
    scan_paths 1 "${CHART_VALUES_DIRECTORY}" || fail "values fixture scan failed"
    scan_paths 1 "${rendered_directory}" || fail "rendered manifest scan failed"
}

run_negative_checks() {
    if [[ ! -f "${NEGATIVE_CASES_FILE}" ]]; then
        fail "missing negative case contract"
    fi

    while IFS= read -r case_line || [[ -n "${case_line}" ]]; do
        case_line="${case_line%$'\r'}"
        [[ -z "${case_line}" || "${case_line}" == \#* ]] && continue

        local expected_category fixture_name fixture_path fixture_output actual_categories
        read -r expected_category fixture_name <<<"${case_line}"
        if [[ -z "${expected_category:-}" || -z "${fixture_name:-}" ]]; then
            fail "invalid negative case contract"
        fi
        fixture_path="${NEGATIVE_FIXTURE_DIRECTORY}/${fixture_name}"
        if fixture_output="$(scan_paths 0 "${fixture_path}" 2>&1)"; then
            fail "negative fixture unexpectedly passed: ${fixture_name}"
        fi
        actual_categories="$(grep -oE 'SECRET[0-9]{3}' <<<"${fixture_output}" | sort -u | tr '\n' ' ')"
        actual_categories="${actual_categories% }"
        if [[ "${actual_categories}" != "${expected_category}" ]]; then
            fail "negative fixture ${fixture_name} reported ${actual_categories:-no stable category} instead of ${expected_category}"
        fi
        printf 'Rejected %s with %s\n' "${fixture_name}" "${expected_category}" >&2
    done <"${NEGATIVE_CASES_FILE}"
}

main() {
    for command in find grep python3 sort tr yq; do
        require_command "${command}"
    done
    if [[ ! -f "${STRUCTURAL_SCANNER}" ]]; then
        fail "missing structural scanner"
    fi

    if [[ ${1:-} == --test ]]; then
        shift
        if [[ $# -ne 2 ]]; then
            usage
        fi
        run_positive_checks "$1" "$2"
        run_negative_checks
        return
    fi
    if [[ $# -lt 1 ]]; then
        usage
    fi
    scan_paths 1 "$@"
}

main "$@"
