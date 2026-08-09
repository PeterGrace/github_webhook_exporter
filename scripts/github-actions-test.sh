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
    require_fragment(file_path, "ghcr.io/petergrace/github-webhook-exporter")
    require_fragment(file_path, "vMAJOR.MINOR.PATCH")
    require_fragment(file_path, "validation-only")
    require_fragment(file_path, "immutable")
    require_fragment(file_path, "`latest`")

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
    fail("workflow branch push trigger must target main only")

if push_section.get("tags") != ["v*"]:
    fail("workflow tag push trigger must target version-like tags for strict validation")

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

if list(jobs) != ["validate", "publish-image"]:
    fail("workflow must define validation followed by image publication")

validate_job = jobs["validate"]
if not isinstance(validate_job, dict):
    fail("workflow validate job must be a mapping")

if validate_job.get("runs-on") != "ubuntu-24.04":
    fail("workflow must run validation on ubuntu-24.04")

job_env = validate_job.get("env")
if not isinstance(job_env, dict):
    fail("workflow validate job must define env")

if job_env.get("CONTAINER_IMAGE") != "github-webhook-exporter:ci":
    fail("workflow must set CONTAINER_IMAGE=github-webhook-exporter:ci")

if job_env.get("KIND_ARTIFACT_DIRECTORY") != "dist/kind-lifecycle":
    fail("workflow must use the fixed Kind lifecycle artifact directory")

validate_steps = validate_job.get("steps")
if not isinstance(validate_steps, list):
    fail("workflow validate job must define steps")

expected_validate_steps = [
    {"uses": "actions/checkout@11d5960a326750d5838078e36cf38b85af677262"},
    {"run": 'scripts/install-ci-tools.sh "$RUNNER_TEMP/ci-tools"'},
    {"run": 'echo "$RUNNER_TEMP/ci-tools" >> "$GITHUB_PATH"'},
    {"uses": "Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6"},
    {"run": "just workflow-test"},
    {"run": "mapfile -t shell_files < <(git ls-files -- '*.sh')\nshellcheck \"${shell_files[@]}\"\n"},
    {"run": "just helm-static"},
    {"run": "just image-smoke"},
    {"run": "just helm-kind-lifecycle"},
    {"run": "just fmt"},
    {"run": "cargo build --locked"},
    {"run": "cargo clippy --all-targets -- -D warnings"},
    {"run": "just test"},
    {"run": "cargo doc --no-deps --locked"},
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

if len(validate_steps) != len(expected_validate_steps):
    fail("workflow must contain the expected validation steps only")

for index, expected_step in enumerate(expected_validate_steps):
    step = validate_steps[index]
    if not isinstance(step, dict):
        fail(f"workflow validation step {index + 1} must be a mapping")
    actual_contract = {key: value for key, value in step.items() if key != "name"}
    if actual_contract != expected_step:
        fail(f"workflow validation step {index + 1} does not match the expected contract")

publish_job = jobs["publish-image"]
if not isinstance(publish_job, dict):
    fail("workflow publish-image job must be a mapping")

expected_publish_job_contract = {
    "if": "startsWith(github.ref, 'refs/tags/v')",
    "needs": "validate",
    "runs-on": "ubuntu-24.04",
    "permissions": {"contents": "read", "packages": "write"},
}
for key, expected_value in expected_publish_job_contract.items():
    if publish_job.get(key) != expected_value:
        fail(f"workflow publish-image job has an invalid {key} contract")

publish_steps = publish_job.get("steps")
if not isinstance(publish_steps, list):
    fail("workflow publish-image job must define steps")

expected_publish_steps = [
    {"uses": "actions/checkout@11d5960a326750d5838078e36cf38b85af677262"},
    {
        "id": "version",
        "run": 'version="$(scripts/release-version.sh "$GITHUB_REF_NAME")"\necho "version=${version}" >> "$GITHUB_OUTPUT"\n',
    },
    {"uses": "docker/setup-buildx-action@8d2750c68a42422c14e847fe6c8ac0403b4cbd6f"},
    {
        "id": "metadata",
        "uses": "docker/metadata-action@c299e40c65443455700f0fdfc63efafe5b349051",
        "with": {
            "images": "ghcr.io/petergrace/github-webhook-exporter",
            "tags": "type=raw,value=${{ steps.version.outputs.version }}",
            "flavor": "latest=false",
        },
    },
    {
        "uses": "docker/build-push-action@10e90e3645eae34f1e60eeb005ba3a3d33f178e8",
        "with": {
            "context": ".",
            "platforms": "linux/amd64",
            "load": True,
            "push": False,
            "tags": "${{ steps.metadata.outputs.tags }}",
            "labels": "${{ steps.metadata.outputs.labels }}",
            "cache-from": "type=gha,scope=production-image",
            "cache-to": "type=gha,mode=max,scope=production-image",
        },
    },
    {
        "env": {"RELEASE_IMAGE": "${{ steps.metadata.outputs.tags }}"},
        "run": 'scripts/container-smoke.sh "$RELEASE_IMAGE"',
    },
    {
        "uses": "docker/login-action@c94ce9fb468520275223c153574b00df6fe4bcc9",
        "with": {
            "registry": "ghcr.io",
            "username": "${{ github.actor }}",
            "password": "${{ secrets.GITHUB_TOKEN }}",
        },
    },
    {
        "env": {"RELEASE_IMAGE": "${{ steps.metadata.outputs.tags }}"},
        "run": "if docker manifest inspect \"$RELEASE_IMAGE\" >/dev/null 2>&1; then\n    printf 'release image already exists: %s\\n' \"$RELEASE_IMAGE\" >&2\n    exit 1\nfi\n",
    },
    {
        "env": {"RELEASE_IMAGE": "${{ steps.metadata.outputs.tags }}"},
        "run": 'docker push "$RELEASE_IMAGE"',
    },
]

if len(publish_steps) != len(expected_publish_steps):
    fail("workflow must contain the expected publication steps only")

for index, expected_step in enumerate(expected_publish_steps):
    step = publish_steps[index]
    if not isinstance(step, dict):
        fail(f"workflow publication step {index + 1} must be a mapping")
    actual_contract = {key: value for key, value in step.items() if key != "name"}
    if actual_contract != expected_step:
        fail(f"workflow publication step {index + 1} does not match the expected contract")
PY
