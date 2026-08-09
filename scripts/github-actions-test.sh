#!/usr/bin/env bash
set -Eeuo pipefail

readonly WORKFLOW_PATH="${1:-}"

fail() {
    printf 'GitHub Actions workflow contract failed: %s\n' "$1" >&2
    exit 1
}

usage() {
    printf 'usage: %s WORKFLOW_PATH\n' "${0##*/}" >&2
    exit 2
}

require_command() {
    local command_name="$1"
    if ! command -v "${command_name}" >/dev/null 2>&1; then
        fail "required command not found: ${command_name}"
    fi
}

if [[ -z "${WORKFLOW_PATH}" ]]; then
    usage
fi

if [[ ! -f "${WORKFLOW_PATH}" ]]; then
    fail "missing workflow file: ${WORKFLOW_PATH}"
fi

require_command yq
require_command python3

python3 - "${WORKFLOW_PATH}" <<'PY'
import json
import subprocess
import sys

workflow_path = sys.argv[1]
result = subprocess.run(
    ["yq", "eval", "-o=json", ".", workflow_path],
    check=True,
    capture_output=True,
    text=True,
)
workflow = json.loads(result.stdout)


def fail(message: str) -> None:
    raise SystemExit(message)


def require_fragment(file_path: str, fragment: str) -> None:
    with open(file_path, encoding="utf-8") as file_handle:
        contents = file_handle.read()
    if fragment not in contents:
        fail(f"missing required documentation reference in {file_path}: {fragment}")


for file_path in (
    "charts/github-webhook-exporter/README.md",
    "docs/operations.md",
):
    require_fragment(file_path, "just helm-static")
    require_fragment(file_path, "just image-smoke")
    require_fragment(file_path, "dist/github-webhook-exporter-0.1.0.tgz")
    require_fragment(file_path, "1.31.0 through 1.35.0")
    require_fragment(file_path, ">=1.31.0-0 <1.36.0-0")
    require_fragment(
        file_path,
        "passing static checks does not prove cluster lifecycle behavior",
    )

require_fragment("docs/operations.md", "`just helm-render` first")

if not isinstance(workflow, dict):
    fail("workflow did not parse as a mapping")

on_section = workflow.get("on")
if not isinstance(on_section, dict):
    fail("workflow must define pull_request and push triggers")

if "pull_request" not in on_section:
    fail("workflow must trigger on pull_request")

if "push" not in on_section:
    fail("workflow must define a push trigger")

push_section = on_section["push"]
if not isinstance(push_section, dict):
    fail("workflow push trigger must be a mapping")

if push_section.get("branches") != ["main"]:
    fail("workflow push trigger must target main only")

permissions = workflow.get("permissions")
if permissions != {"contents": "read"}:
    fail("workflow must use least-privilege contents: read permissions")

concurrency = workflow.get("concurrency")
expected_concurrency = {
    "group": "${{ github.workflow }}-${{ github.ref }}",
    "cancel-in-progress": True,
}
if concurrency != expected_concurrency:
    fail("workflow concurrency must cancel stale branch runs")

jobs = workflow.get("jobs")
if not isinstance(jobs, dict):
    fail("workflow must define jobs")

if list(jobs) != ["validate"]:
    fail("workflow must define exactly one validate job")

job = jobs["validate"]
if not isinstance(job, dict):
    fail("workflow validate job must be a mapping")

if job.get("runs-on") != "ubuntu-24.04":
    fail("workflow must run on ubuntu-24.04")

job_env = job.get("env")
if not isinstance(job_env, dict):
    fail("workflow validate job must define env")

if job_env.get("CONTAINER_IMAGE") != "github-webhook-exporter:ci":
    fail("workflow must set CONTAINER_IMAGE=github-webhook-exporter:ci")

if job_env.get("KIND_ARTIFACT_DIRECTORY") != "dist/kind-lifecycle":
    fail("workflow must use the fixed Kind lifecycle artifact directory")

steps = job.get("steps")
if not isinstance(steps, list):
    fail("workflow validate job must define steps")

expected_steps = [
    {
        "uses": "actions/checkout@11d5960a326750d5838078e36cf38b85af677262",
    },
    {
        "run": 'scripts/install-ci-tools.sh "$RUNNER_TEMP/ci-tools"',
    },
    {
        "run": 'echo "$RUNNER_TEMP/ci-tools" >> "$GITHUB_PATH"',
    },
    {
        "uses": "Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6",
    },
    {
        "run": "just workflow-test",
    },
    {
        "run": "mapfile -t shell_files < <(git ls-files -- '*.sh')\nshellcheck \"${shell_files[@]}\"\n",
    },
    {
        "run": "just helm-static",
    },
    {
        "run": "just image-smoke",
    },
    {
        "run": "just helm-kind-lifecycle",
    },
    {
        "run": "just fmt",
    },
    {
        "run": "cargo build --locked",
    },
    {
        "run": "cargo clippy --all-targets -- -D warnings",
    },
    {
        "run": "just test",
    },
    {
        "run": "cargo doc --no-deps --locked",
    },
    {
        "uses": "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02",
        "with": {
            "name": "helm-package",
            "path": "dist/github-webhook-exporter-0.1.0.tgz",
            "if-no-files-found": "error",
            "retention-days": 30,
        },
    },
    {
        "if": "always()",
        "uses": "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02",
        "with": {
            "name": "kind-lifecycle-diagnostics",
            "path": "dist/kind-lifecycle",
            "if-no-files-found": "warn",
            "retention-days": 14,
        },
    },
]

if len(steps) != len(expected_steps):
    fail("workflow must contain the expected validation steps only")

cargo_doc_index = None
package_upload_index = None
diagnostics_upload_index = None

for index, expected_step in enumerate(expected_steps):
    step = steps[index]
    if not isinstance(step, dict):
        fail(f"workflow step {index + 1} must be a mapping")

    actual_contract = {key: value for key, value in step.items() if key != "name"}
    if actual_contract != expected_step:
        fail(f"workflow step {index + 1} does not match the expected contract")

    if expected_step.get("run") == "cargo doc --no-deps --locked":
        cargo_doc_index = index
    with_section = expected_step.get("with", {})
    if with_section.get("name") == "helm-package":
        package_upload_index = index
    if with_section.get("name") == "kind-lifecycle-diagnostics":
        diagnostics_upload_index = index

if cargo_doc_index is None:
    fail("workflow must generate documentation")
if package_upload_index is None or package_upload_index <= cargo_doc_index:
    fail("workflow must upload the packaged archive after cargo doc")
if diagnostics_upload_index is None or diagnostics_upload_index <= package_upload_index:
    fail("workflow must upload Kind diagnostics after the packaged archive")
PY
