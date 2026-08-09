#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY
readonly REPOSITORY_ROOT="${SCRIPT_DIRECTORY}/.."
readonly OUTPUT_DIRECTORY_TEST="${SCRIPT_DIRECTORY}/helm-output-directory-test.py"
TEMPORARY_DIRECTORY=""
FAILURE_PROBE_INDEX=0

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

expect_failure() {
    local expected_diagnostic="$1"
    local probe_name="$2"
    shift 2

    ((FAILURE_PROBE_INDEX += 1))
    local error_file="${TEMPORARY_DIRECTORY}/failure-${FAILURE_PROBE_INDEX}.stderr"
    if "$@" >/dev/null 2>"${error_file}"; then
        fail "${probe_name} unexpectedly succeeded"
    fi

    local actual_diagnostic
    actual_diagnostic="$(<"${error_file}")"
    if [[ "${actual_diagnostic}" != "${expected_diagnostic}" ]]; then
        fail "${probe_name} returned an unexpected diagnostic"
    fi
}

TEMPORARY_DIRECTORY="$(mktemp -d)"

# Prove the assertion harness rejects a nonzero exit carrying the wrong reason.
if (expect_failure "expected diagnostic" "diagnostic assertion probe" \
    bash -c 'printf "%s\n" "unrelated diagnostic" >&2; exit 1') \
    >/dev/null 2>"${TEMPORARY_DIRECTORY}/assertion-helper-error"; then
    fail "diagnostic assertion helper accepted an unrelated failure"
fi
ASSERTION_HELPER_ERROR="Helm security self-test failed: diagnostic assertion probe"
ASSERTION_HELPER_ERROR+=" returned an unexpected diagnostic"
readonly ASSERTION_HELPER_ERROR
if [[ "$(<"${TEMPORARY_DIRECTORY}/assertion-helper-error")" != \
      "${ASSERTION_HELPER_ERROR}" ]]; then
    fail "diagnostic assertion helper returned an unstable error"
fi
printf 'Diagnostic assertion helper fixture passed\n'

valuable_directory="${TEMPORARY_DIRECTORY}/valuable"
mkdir "${valuable_directory}"
printf '%s\n' 'preserve-me' >"${valuable_directory}/sentinel"

expect_failure "Helm render matrix failed: unsafe output directory" \
    "render unowned output guard" \
    "${SCRIPT_DIRECTORY}/helm-render-matrix.sh" \
    "${REPOSITORY_ROOT}/charts/github-webhook-exporter" "${valuable_directory}"
expect_failure "Helm package test failed: unsafe output directory" \
    "package unowned output guard" \
    "${SCRIPT_DIRECTORY}/helm-package-test.sh" \
    "${REPOSITORY_ROOT}/charts/github-webhook-exporter" "${valuable_directory}"
if [[ "$(<"${valuable_directory}/sentinel")" != "preserve-me" ]]; then
    fail "output guard modified caller data"
fi
if grep -Fq 'preserve-me' "${TEMPORARY_DIRECTORY}"/failure-*.stderr; then
    fail "output guard disclosed caller data"
fi

expect_failure "Helm render matrix failed: unsafe output directory" \
    "repository output guard" \
    "${SCRIPT_DIRECTORY}/helm-render-matrix.sh" \
    "${REPOSITORY_ROOT}/charts/github-webhook-exporter" "${REPOSITORY_ROOT}"
expect_failure "Helm render matrix failed: unsafe output directory" \
    "chart descendant output guard" \
    "${SCRIPT_DIRECTORY}/helm-render-matrix.sh" \
    "${REPOSITORY_ROOT}/charts/github-webhook-exporter" \
    "${REPOSITORY_ROOT}/charts/github-webhook-exporter/generated"
python3 "${OUTPUT_DIRECTORY_TEST}" >/dev/null
printf 'Output-directory guard fixtures passed\n'

python3 - "${TEMPORARY_DIRECTORY}" <<'PY'
import io
import pathlib
import sys
import tarfile

root = pathlib.Path(sys.argv[1])
fixtures = {
    "traversal.tgz": [("chart/../../private", tarfile.REGTYPE, b"")],
    "absolute.tgz": [("/private", tarfile.REGTYPE, b"")],
    "symlink.tgz": [("chart/link", tarfile.SYMTYPE, b"")],
    "hardlink.tgz": [("chart/link", tarfile.LNKTYPE, b"")],
    "collision.tgz": [
        ("chart/file", tarfile.REGTYPE, b"first"),
        ("chart/./file", tarfile.REGTYPE, b"second"),
    ],
}
for archive_name, entries in fixtures.items():
    with tarfile.open(root / archive_name, mode="w:gz") as archive:
        for member_name, member_type, contents in entries:
            member = tarfile.TarInfo(member_name)
            member.type = member_type
            member.size = len(contents)
            if member_type == tarfile.SYMTYPE:
                member.linkname = "/tmp/target"
            elif member_type == tarfile.LNKTYPE:
                member.linkname = "chart/Chart.yaml"
            archive.addfile(member, io.BytesIO(contents) if contents else None)
PY
expect_failure "Helm archive preflight failed: ARCHIVE001" \
    "archive traversal guard" python3 "${SCRIPT_DIRECTORY}/helm-archive-preflight.py" \
    "${TEMPORARY_DIRECTORY}/traversal.tgz" chart
expect_failure "Helm archive preflight failed: ARCHIVE001" \
    "archive absolute-path guard" python3 "${SCRIPT_DIRECTORY}/helm-archive-preflight.py" \
    "${TEMPORARY_DIRECTORY}/absolute.tgz" chart
expect_failure "Helm archive preflight failed: ARCHIVE005" \
    "archive symlink guard" python3 "${SCRIPT_DIRECTORY}/helm-archive-preflight.py" \
    "${TEMPORARY_DIRECTORY}/symlink.tgz" chart
expect_failure "Helm archive preflight failed: ARCHIVE005" \
    "archive hardlink guard" python3 "${SCRIPT_DIRECTORY}/helm-archive-preflight.py" \
    "${TEMPORARY_DIRECTORY}/hardlink.tgz" chart
expect_failure "Helm archive preflight failed: ARCHIVE004" \
    "archive normalized-collision guard" \
    python3 "${SCRIPT_DIRECTORY}/helm-archive-preflight.py" \
    "${TEMPORARY_DIRECTORY}/collision.tgz" chart
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
expect_failure "Helm render comparison failed: RENDER003" \
    "source/archive render mismatch guard" \
    python3 "${SCRIPT_DIRECTORY}/helm-render-compare.py" "${source_rendered}" \
    "${archive_rendered}" "${REPOSITORY_ROOT}/ci/helm/render-cases.txt"
printf 'Source/archive byte comparison fixture passed\n'

install_directory="${TEMPORARY_DIRECTORY}/install"
mkdir "${install_directory}"
printf '%s\n' 'unchanged' >"${TEMPORARY_DIRECTORY}/installer-victim"
ln -s "${TEMPORARY_DIRECTORY}/installer-victim" "${install_directory}/helm"
expect_failure "CI tool installer failed: installation target already exists: helm" \
    "installer symlink guard" \
    "${SCRIPT_DIRECTORY}/install-ci-tools.sh" "${install_directory}"
if [[ "$(<"${TEMPORARY_DIRECTORY}/installer-victim")" != "unchanged" ]]; then
    fail "installer followed a symlink target"
fi
printf 'Installer symlink fixture passed\n'

workflow_copy="${TEMPORARY_DIRECTORY}/workflow.yml"
python3 - "${REPOSITORY_ROOT}/.github/workflows/helm-package-ci.yml" \
    "${workflow_copy}" <<'PY'
import pathlib
import sys
source, destination = map(pathlib.Path, sys.argv[1:])
contents = source.read_text(encoding="utf-8")
contents = contents.replace("run: just workflow-test", "run: echo contract-skipped", 1)
destination.write_text(contents, encoding="utf-8")
PY
expect_failure "workflow step 5 does not match the expected contract" \
    "workflow mutation guard" \
    "${SCRIPT_DIRECTORY}/github-actions-test.sh" "${workflow_copy}"
printf 'Workflow order fixture passed\n'
