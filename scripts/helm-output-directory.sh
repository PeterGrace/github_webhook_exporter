#!/usr/bin/env bash

# Shared output-directory guard for Helm generation scripts. This file is sourced.

HELM_OUTPUT_GUARD_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly HELM_OUTPUT_GUARD_DIRECTORY
readonly HELM_OUTPUT_COMMIT_HELPER="${HELM_OUTPUT_GUARD_DIRECTORY}/helm-output-commit.py"

helm_output_fail() {
    printf '%s failed: unsafe output directory\n' "${HELM_OUTPUT_ERROR_PREFIX:-Helm output}" >&2
    return 1
}

helm_output_prepare() {
    local output_kind="$1"
    local chart_directory="$2"
    local requested_output="$3"
    local repository_root="$4"

    if [[ -z "${requested_output}" || "${requested_output}" == *$'\n'* ||
          "${requested_output}" == *$'\r'* ]]; then
        helm_output_fail
        return 1
    fi

    local path_result
    if ! path_result="$(python3 - "${chart_directory}" "${requested_output}" \
        "${repository_root}" <<'PY'
import os
import pathlib
import sys

chart_raw, output_raw, repository_raw = sys.argv[1:]
chart = pathlib.Path(chart_raw).resolve(strict=True)
repository = pathlib.Path(repository_raw).resolve(strict=True)
output_absolute = pathlib.Path(os.path.abspath(output_raw))
output = output_absolute.resolve(strict=False)

# Reject symlinked parent components and ambiguous lexical spellings.
if output != output_absolute:
    raise SystemExit(1)


def contains(parent: pathlib.Path, child: pathlib.Path) -> bool:
    try:
        child.relative_to(parent)
    except ValueError:
        return False
    return True

# Generated output must be wholly separate from the repository/chart and cannot
# replace an ancestor of either one.
if (
    contains(output, repository)
    or (contains(repository, output) and not contains(repository / "dist", output))
    or contains(output, chart)
    or contains(chart, output)
):
    raise SystemExit(1)

print(output)
print(repository)
PY
    )"; then
        helm_output_fail
        return 1
    fi

    mapfile -t helm_output_paths <<<"${path_result}"
    HELM_OUTPUT_DIRECTORY="${helm_output_paths[0]}"
    local canonical_repository="${helm_output_paths[1]}"
    HELM_OUTPUT_PARENT="${HELM_OUTPUT_DIRECTORY%/*}"
    HELM_OUTPUT_NAME="${HELM_OUTPUT_DIRECTORY##*/}"
    HELM_OUTPUT_MARKER="${HELM_OUTPUT_DIRECTORY}/.gwe-generated-output"
    HELM_OUTPUT_ALLOW_GENERATED_ROOT=0
    HELM_OUTPUT_STAGE=""
    HELM_OUTPUT_SNAPSHOT=()
    if [[ "${HELM_OUTPUT_DIRECTORY}" == "${canonical_repository}/dist" ]]; then
        HELM_OUTPUT_ALLOW_GENERATED_ROOT=1
    fi

    if [[ -L "${HELM_OUTPUT_DIRECTORY}" ]]; then
        helm_output_fail
        return 1
    fi

    if [[ -e "${HELM_OUTPUT_DIRECTORY}" ]]; then
        local owned_output=0
        if [[ -d "${HELM_OUTPUT_DIRECTORY}" && ! -L "${HELM_OUTPUT_MARKER}" &&
              -f "${HELM_OUTPUT_MARKER}" &&
              "$(<"${HELM_OUTPUT_MARKER}")" == "${output_kind}" ]]; then
            owned_output=1
        elif [[ "${HELM_OUTPUT_DIRECTORY}" == "${canonical_repository}/dist" &&
                ! -L "${HELM_OUTPUT_DIRECTORY}/.gwe-generated-root" &&
                -f "${HELM_OUTPUT_DIRECTORY}/.gwe-generated-root" &&
                "$(<"${HELM_OUTPUT_DIRECTORY}/.gwe-generated-root")" == \
                    "gwe-generated-root" ]]; then
            owned_output=1
        fi
        if (( ! owned_output )); then
            helm_output_fail
            return 1
        fi
    fi

    if [[ ! -d "${HELM_OUTPUT_PARENT}" ]]; then
        case "${HELM_OUTPUT_DIRECTORY}" in
            "${canonical_repository}/dist"/*)
                mkdir -p -- "${HELM_OUTPUT_PARENT}"
                printf '%s\n' "gwe-generated-root" \
                    >"${canonical_repository}/dist/.gwe-generated-root"
                ;;
            *)
                helm_output_fail
                return 1
                ;;
        esac
    fi

    if [[ -L "${HELM_OUTPUT_PARENT}" || ! -d "${HELM_OUTPUT_PARENT}" ||
          ! -f "${HELM_OUTPUT_COMMIT_HELPER}" ]]; then
        helm_output_fail
        return 1
    fi

    HELM_OUTPUT_STAGE="$(mktemp -d \
        "${HELM_OUTPUT_PARENT}/.${HELM_OUTPUT_NAME}.stage.XXXXXX")"
    printf '%s\n' "${output_kind}" >"${HELM_OUTPUT_STAGE}/.gwe-generated-output"

    local snapshot_result
    if ! snapshot_result="$(python3 "${HELM_OUTPUT_COMMIT_HELPER}" snapshot \
        "${HELM_OUTPUT_PARENT}" "${HELM_OUTPUT_NAME}" "${HELM_OUTPUT_STAGE##*/}" \
        "${output_kind}" "${HELM_OUTPUT_ALLOW_GENERATED_ROOT}")"; then
        helm_output_fail
        return 1
    fi
    read -r -a HELM_OUTPUT_SNAPSHOT <<<"${snapshot_result}"
    if [[ ${#HELM_OUTPUT_SNAPSHOT[@]} -ne 7 ]]; then
        helm_output_fail
        return 1
    fi
    HELM_OUTPUT_KIND="${output_kind}"
}

helm_output_commit() {
    if ! python3 "${HELM_OUTPUT_COMMIT_HELPER}" commit \
        "${HELM_OUTPUT_PARENT}" "${HELM_OUTPUT_NAME}" "${HELM_OUTPUT_STAGE##*/}" \
        "${HELM_OUTPUT_KIND}" "${HELM_OUTPUT_ALLOW_GENERATED_ROOT}" \
        "${HELM_OUTPUT_SNAPSHOT[@]}"; then
        helm_output_fail
        return 1
    fi
    HELM_OUTPUT_STAGE=""
}

helm_output_cleanup_stage() {
    if [[ -n "${HELM_OUTPUT_STAGE:-}" && ${#HELM_OUTPUT_SNAPSHOT[@]} -eq 7 ]]; then
        if python3 "${HELM_OUTPUT_COMMIT_HELPER}" cleanup \
            "${HELM_OUTPUT_PARENT}" "${HELM_OUTPUT_STAGE##*/}" \
            "${HELM_OUTPUT_SNAPSHOT[@]}"; then
            HELM_OUTPUT_STAGE=""
        fi
    fi
}
