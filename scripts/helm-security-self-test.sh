#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY
readonly REPOSITORY_ROOT="${SCRIPT_DIRECTORY}/.."
TEMPORARY_DIRECTORY=""

fail() {
    printf 'Helm security self-test failed: %s\n' "$1" >&2
    exit 1
}

cleanup() {
    if [[ -n "${TEMPORARY_DIRECTORY}" ]]; then
        rm -rf -- "${TEMPORARY_DIRECTORY}"
    fi
}
trap cleanup EXIT

TEMPORARY_DIRECTORY="$(mktemp -d)"
valuable_directory="${TEMPORARY_DIRECTORY}/valuable"
mkdir "${valuable_directory}"
printf '%s\n' 'preserve-me' >"${valuable_directory}/sentinel"

if "${SCRIPT_DIRECTORY}/helm-render-matrix.sh" \
    "${REPOSITORY_ROOT}/charts/github-webhook-exporter" "${valuable_directory}" \
    >/dev/null 2>"${TEMPORARY_DIRECTORY}/render-error"; then
    fail "render output guard accepted an unowned directory"
fi
if "${SCRIPT_DIRECTORY}/helm-package-test.sh" \
    "${REPOSITORY_ROOT}/charts/github-webhook-exporter" "${valuable_directory}" \
    >/dev/null 2>"${TEMPORARY_DIRECTORY}/package-error"; then
    fail "package output guard accepted an unowned directory"
fi
if [[ "$(<"${valuable_directory}/sentinel")" != "preserve-me" ]]; then
    fail "output guard modified caller data"
fi
if grep -Fq 'preserve-me' "${TEMPORARY_DIRECTORY}/render-error" \
    "${TEMPORARY_DIRECTORY}/package-error"; then
    fail "output guard disclosed caller data"
fi

if "${SCRIPT_DIRECTORY}/helm-render-matrix.sh" \
    "${REPOSITORY_ROOT}/charts/github-webhook-exporter" "${REPOSITORY_ROOT}" \
    >/dev/null 2>&1; then
    fail "render output guard accepted the repository root"
fi
if "${SCRIPT_DIRECTORY}/helm-render-matrix.sh" \
    "${REPOSITORY_ROOT}/charts/github-webhook-exporter" \
    "${REPOSITORY_ROOT}/charts/github-webhook-exporter/generated" >/dev/null 2>&1; then
    fail "render output guard accepted a chart descendant"
fi
printf 'Output-directory guard fixtures passed\n'

python3 "${SCRIPT_DIRECTORY}/helm-archive-preflight-test.py" >/dev/null
printf 'Archive member attack fixtures passed\n'

source_rendered="${TEMPORARY_DIRECTORY}/source-rendered"
archive_rendered="${TEMPORARY_DIRECTORY}/archive-rendered"
mkdir "${source_rendered}" "${archive_rendered}"
while IFS= read -r case_name || [[ -n "${case_name}" ]]; do
    [[ -z "${case_name}" ]] && continue
    printf '%s\n' 'identical-render' >"${source_rendered}/${case_name}.yaml"
    printf '%s\n' 'identical-render' >"${archive_rendered}/${case_name}.yaml"
done <"${REPOSITORY_ROOT}/ci/helm/render-cases.txt"
python3 "${SCRIPT_DIRECTORY}/helm-render-compare.py" "${source_rendered}" \
    "${archive_rendered}" "${REPOSITORY_ROOT}/ci/helm/render-cases.txt"
printf '%s\n' 'packaging-omission' >"${archive_rendered}/default.yaml"
if python3 "${SCRIPT_DIRECTORY}/helm-render-compare.py" "${source_rendered}" \
    "${archive_rendered}" "${REPOSITORY_ROOT}/ci/helm/render-cases.txt" >/dev/null 2>&1; then
    fail "render comparison accepted differing archive output"
fi
printf 'Source/archive byte comparison fixture passed\n'

install_directory="${TEMPORARY_DIRECTORY}/install"
mkdir "${install_directory}"
printf '%s\n' 'unchanged' >"${TEMPORARY_DIRECTORY}/installer-victim"
ln -s "${TEMPORARY_DIRECTORY}/installer-victim" "${install_directory}/helm"
if "${SCRIPT_DIRECTORY}/install-ci-tools.sh" "${install_directory}" \
    >/dev/null 2>"${TEMPORARY_DIRECTORY}/installer-error"; then
    fail "installer accepted a symlink target"
fi
if [[ "$(<"${TEMPORARY_DIRECTORY}/installer-victim")" != "unchanged" ]]; then
    fail "installer followed a symlink target"
fi
printf 'Installer symlink fixture passed\n'

workflow_copy="${TEMPORARY_DIRECTORY}/workflow.yml"
python3 - "${REPOSITORY_ROOT}/.github/workflows/helm-package-ci.yml" "${workflow_copy}" <<'PY'
import pathlib
import sys
source, destination = map(pathlib.Path, sys.argv[1:])
contents = source.read_text(encoding="utf-8")
contents = contents.replace("run: just workflow-test", "run: echo contract-skipped", 1)
destination.write_text(contents, encoding="utf-8")
PY
if "${SCRIPT_DIRECTORY}/github-actions-test.sh" "${workflow_copy}" >/dev/null 2>&1; then
    fail "workflow contract accepted a missing in-CI workflow test"
fi
printf 'Workflow order fixture passed\n'
