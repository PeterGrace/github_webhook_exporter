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
import re
import subprocess
import sys

workflow_path = sys.argv[1]

CHART_METADATA = "charts/github-webhook-exporter/Chart.yaml"
CI_TOOL_VERSIONS = "ci/tool-versions.env"
CI_TOOL_INSTALLER = "scripts/install-ci-tools.sh"


def fail(message: str) -> None:
    raise SystemExit(message)


def chart_version() -> str:
    """Read the declared chart version so a release bump needs no edit here."""
    with open(CHART_METADATA, encoding="utf-8") as file_handle:
        match = re.search(r"^version:\s*(\S+)$", file_handle.read(), re.MULTILINE)
    if match is None:
        fail(f"missing chart version in {CHART_METADATA}")
    return match.group(1)


def rust_version() -> str:
    """Read the single pinned Rust version shared by every CI setup path."""
    with open(CI_TOOL_VERSIONS, encoding="utf-8") as file_handle:
        matches = re.findall(
            r"^RUST_VERSION=([0-9]+\.[0-9]+\.[0-9]+)$",
            file_handle.read(),
            re.MULTILINE,
        )
    if len(matches) != 1:
        fail(f"expected exactly one valid RUST_VERSION in {CI_TOOL_VERSIONS}")
    return matches[0]


def rustup_install_options(contents: str, source: str) -> str:
    """Extract normalized rustup profile and component options from a setup path."""
    normalized = " ".join(contents.replace("\\\n", " ").split())
    match = re.search(
        r'rustup -q toolchain install "[^\"]+" '
        r"(?P<options>--profile \S+(?: --component \S+)+ --no-self-update)",
        normalized,
    )
    if match is None:
        fail(f"missing pinned rustup install contract in {source}")
    return match.group("options")


CHART_VERSION = chart_version()
RUST_VERSION = rust_version()

with open(CI_TOOL_INSTALLER, encoding="utf-8") as file_handle:
    installer_rustup_options = rustup_install_options(
        file_handle.read(), CI_TOOL_INSTALLER
    )

with open("scripts/helm-package-test.sh", encoding="utf-8") as file_handle:
    package_test = file_handle.read()
if re.search(
    r'readonly PACKAGE_NAME="github-webhook-exporter-\d+\.\d+\.\d+\.tgz"', package_test
):
    fail("Helm package validation must not hard-code the current chart version")

with open("scripts/image-reproducibility-test.sh", encoding="utf-8") as file_handle:
    reproducibility_test = file_handle.read()
if "docker buildx build" not in reproducibility_test or "--load" not in reproducibility_test:
    fail("image reproducibility test must use the release buildx load path")
if "--no-cache" not in reproducibility_test:
    fail("image reproducibility test must disable cache with --no-cache")
if len(re.findall(r'^\s*build_image\s+"', reproducibility_test, re.MULTILINE)) != 2:
    fail("image reproducibility test must call build_image exactly twice")


result = subprocess.run(
    ["yq", "eval", "-o=json", ".", workflow_path],
    check=True,
    capture_output=True,
    text=True,
)
workflow = json.loads(result.stdout)


def require_fragment(file_path: str, fragment: str) -> None:
    with open(file_path, encoding="utf-8") as file_handle:
        contents = file_handle.read()
    normalized_contents = " ".join(contents.split())
    normalized_fragment = " ".join(fragment.split())
    if normalized_fragment not in normalized_contents:
        fail(f"missing required documentation reference in {file_path}: {fragment}")

with open("Dockerfile", encoding="utf-8") as file_handle:
    dockerfile = file_handle.read()

required_dockerfile_fragments = (
    "cargo install cargo-chef --version 0.1.71 --locked",
    "FROM chef AS planner",
    "cargo chef prepare --recipe-path recipe.json",
    "FROM chef AS builder",
    "COPY --from=planner /build/recipe.json recipe.json",
    "cargo chef cook --locked --release --recipe-path recipe.json",
    "cargo build --locked --release",
    "install -D -m 0555 \\\n        target/release/github_webhook_exporter \\\n        /out/usr/local/bin/github_webhook_exporter",
)
for fragment in required_dockerfile_fragments:
    if fragment not in dockerfile:
        fail(f"Dockerfile is missing cache contract: {fragment}")

if "target=/build/target" in dockerfile:
    fail("Dockerfile must not mount /build/target as a BuildKit cache")
if dockerfile.count("cargo build --locked --release") != 1:
    fail("Dockerfile must compile the application exactly once")
if dockerfile.index("cargo chef cook --locked --release") > dockerfile.rindex(
    "COPY migrations/ migrations/"
):
    fail("Dockerfile must cook dependencies before copying application inputs")
if dockerfile.index("cargo build --locked --release") > dockerfile.index(
    "ARG SOURCE_DATE_EPOCH=0"
):
    fail("SOURCE_DATE_EPOCH must not invalidate application compilation")

with open("justfile", encoding="utf-8") as file_handle:
    justfile = file_handle.read()

required_just_fragments = (
    "image-smoke: image-build image-smoke-loaded",
    'image-smoke-loaded:\n    scripts/container-smoke.sh "{{container-image}}"',
    "helm-kind-lifecycle: image-build helm-kind-lifecycle-loaded",
    'helm-kind-lifecycle-loaded:\n    scripts/helm-kind-lifecycle.sh "{{helm-chart}}" "{{container-image}}" \\\n        "${KIND_ARTIFACT_DIRECTORY:-dist/kind-lifecycle}"',
)
for fragment in required_just_fragments:
    if fragment not in justfile:
        fail(f"justfile is missing loaded-image contract: {fragment}")


shared_release_fragments = (
    "ghcr.io/petergrace/github-webhook-exporter",
    "vMAJOR.MINOR.PATCH",
    "validation-only",
    "immutable",
    "overwrite guard is not atomic",
    "`latest`",
    "oci://ghcr.io/petergrace/charts/github-webhook-exporter",
    f"helm pull oci://ghcr.io/petergrace/charts/github-webhook-exporter --version {CHART_VERSION}",
    f"helm install github-webhook-exporter oci://ghcr.io/petergrace/charts/github-webhook-exporter --version {CHART_VERSION}",
    "Published version tags are immutable.",
    "The workflow never publishes `latest`, branch, SHA, or prerelease tags.",
    "Only the image-existing/chart-missing state with an exact matching digest may resume as chart-only recovery.",
    "Completed, chart-only, and digest-conflict states fail closed without overwrite.",
)

for file_path in (
    "charts/github-webhook-exporter/README.md",
    "book/src/reference/release-and-packaging.md",
):
    require_fragment(file_path, "just helm-static")
    require_fragment(file_path, "just image-smoke")
    require_fragment(file_path, "1.31.0 through 1.35.0")
    require_fragment(file_path, ">=1.31.0-0 <1.36.0-0")
    require_fragment(
        file_path,
        "passing static checks does not prove cluster lifecycle behavior",
    )
    for fragment in shared_release_fragments:
        require_fragment(file_path, fragment)

require_fragment("book/src/reference/release-and-packaging.md", "`just helm-render` first")
require_fragment("book/src/reference/release-and-packaging.md", "Image state")
require_fragment("book/src/reference/release-and-packaging.md", "Chart state")
require_fragment(
    "book/src/reference/release-and-packaging.md",
    "If validation fails, rerun the original failed workflow attempt without moving the tag.",
)
require_fragment(
    "book/src/reference/release-and-packaging.md",
    "chart-only registry state fails closed without overwrite.",
)
require_fragment("book/src/reference/release-and-packaging.md", "Existing image tags are never overwritten.")
require_fragment(
    "book/src/reference/release-and-packaging.md",
    (
        "An exact matching existing image permits chart-only recovery only when "
        "the chart is absent."
    ),
)

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

if push_section.get("tags") != ["v[0-9]+.[0-9]+.[0-9]+"]:
    fail("workflow tag trigger must target stable semantic-version-shaped tags")

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

if list(jobs) != ["validate", "publish-release"]:
    fail("workflow must define validation followed by release publication")

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

if job_env.get("RUSTUP_TOOLCHAIN") != RUST_VERSION:
    fail(
        "workflow RUSTUP_TOOLCHAIN must match "
        f"RUST_VERSION={RUST_VERSION} from {CI_TOOL_VERSIONS}"
    )

validate_steps = validate_job.get("steps")
if not isinstance(validate_steps, list):
    fail("workflow validate job must define steps")

rustup_steps = [
    step
    for step in validate_steps
    if isinstance(step, dict) and "rustup -q toolchain install" in step.get("run", "")
]
if len(rustup_steps) != 1:
    fail("workflow must contain exactly one inline rustup install step")
workflow_rustup_options = rustup_install_options(
    rustup_steps[0]["run"], "workflow Rust toolchain step"
)
if workflow_rustup_options != installer_rustup_options:
    fail("workflow and CI tool installer rustup options must match")

expected_validate_steps = [
    {"uses": "actions/checkout@11d5960a326750d5838078e36cf38b85af677262"},
    {
        "id": "ci-tools-cache",
        "uses": "actions/cache@0057852bfaa89a56745cba8c7296529d2fc39830",
        "with": {
            "path": "${{ runner.temp }}/ci-tools",
            "key": "ci-tools-${{ runner.os }}-${{ runner.arch }}-${{ hashFiles('ci/tool-versions.env', 'scripts/install-ci-tools.sh') }}",
        },
    },
    {
        "if": "steps.ci-tools-cache.outputs.cache-hit != 'true'",
        "run": 'scripts/install-ci-tools.sh "$RUNNER_TEMP/ci-tools"',
    },
    {"run": 'echo "$RUNNER_TEMP/ci-tools" >> "$GITHUB_PATH"'},
    {
        "if": "steps.ci-tools-cache.outputs.cache-hit == 'true'",
        "run": "rustup -q toolchain install \"$RUSTUP_TOOLCHAIN\" --profile minimal \\\n    --component rustfmt --component clippy --no-self-update\n",
    },
    {"uses": "Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6"},
    {"run": "just workflow-test"},
    {"run": "just release-flow-test"},
    {"run": "mapfile -t shell_files < <(git ls-files -- '*.sh')\nshellcheck \"${shell_files[@]}\"\n"},
    {"run": "just helm-static"},
    {"uses": "docker/setup-buildx-action@8d2750c68a42422c14e847fe6c8ac0403b4cbd6f"},
    {
        "uses": "docker/build-push-action@10e90e3645eae34f1e60eeb005ba3a3d33f178e8",
        "with": {
            "context": ".",
            "platforms": "linux/amd64",
            "load": True,
            "push": False,
            "provenance": False,
            "tags": "github-webhook-exporter:ci",
            "cache-from": "type=gha,scope=production-image-linux-amd64",
            "cache-to": "type=gha,mode=max,scope=production-image-linux-amd64",
        },
    },
    {"run": "just image-smoke-loaded"},
    {"if": "github.event_name == 'push'", "run": "just image-reproducibility-test"},
    {"run": "just helm-maintenance-unit"},
    {"run": "just helm-kind-lifecycle-loaded"},
    {"run": "just fmt"},
    {"run": "cargo clippy --all-targets -- -D warnings"},
    {"run": "just test"},
    {"run": "cargo doc --no-deps --locked"},
    {
        "uses": "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02",
        "with": {
            "name": "helm-package",
            "path": "dist/github-webhook-exporter-*.tgz",
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

if validate_job.get("permissions", {}).get("packages") == "write":
    fail("workflow validation job must not grant packages: write")

for index, step in enumerate(validate_steps):
    serialized_step = json.dumps(step, sort_keys=True).lower()
    if (
        "login-action" in serialized_step
        or "registry login" in serialized_step
        or "docker push" in serialized_step
        or "helm push" in serialized_step
        or "release-publish.sh" in serialized_step
        or step.get("with", {}).get("push") is True
    ):
        fail(f"workflow validation step {index + 1} must not authenticate or publish")

publish_job = jobs["publish-release"]
if not isinstance(publish_job, dict):
    fail("workflow publish-release job must be a mapping")

expected_publish_job_contract = {
    "if": "github.ref_type == 'tag'",
    "needs": "validate",
    "runs-on": "ubuntu-24.04",
    "permissions": {"contents": "read", "packages": "write"},
}
for key, expected_value in expected_publish_job_contract.items():
    if publish_job.get(key) != expected_value:
        fail(f"workflow publish-release job has an invalid {key} contract")

publish_steps = publish_job.get("steps")
if not isinstance(publish_steps, list):
    fail("workflow publish-release job must define steps")

expected_publish_steps = [
    {"uses": "actions/checkout@11d5960a326750d5838078e36cf38b85af677262"},
    {
        "id": "version",
        "run": 'version="$(scripts/release-version.sh "$GITHUB_REF_NAME")"\ncommit_timestamp="$(git show -s --format=%cI "$GITHUB_SHA")"\nsource_date_epoch="$(git show -s --format=%ct "$GITHUB_SHA")"\nprintf \'version=%s\\ncommit_timestamp=%s\\nsource_date_epoch=%s\\n\' \\\n    "$version" "$commit_timestamp" "$source_date_epoch" >> "$GITHUB_OUTPUT"\n',
    },
    {"run": 'scripts/install-ci-tools.sh "$RUNNER_TEMP/ci-tools"'},
    {"run": 'echo "$RUNNER_TEMP/ci-tools" >> "$GITHUB_PATH"'},
    {
        "uses": "actions/download-artifact@d3f86a106a0bac45b974a628896c90dbdf5c8093",
        "with": {"name": "helm-package", "path": "dist/release"},
    },
    {
        "id": "chart",
        "run": "mapfile -d '' -t chart_archives < <(\n    find dist/release -maxdepth 1 -type f -name '*.tgz' -print0\n)\nif (( ${#chart_archives[@]} != 1 )); then\n    printf 'expected exactly one downloaded chart archive, found %s\\n' \\\n        \"${#chart_archives[@]}\" >&2\n    exit 1\nfi\n\narchive=\"${chart_archives[0]}\"\nexpected_archive=\"dist/release/github-webhook-exporter-${{ steps.version.outputs.version }}.tgz\"\nif [[ \"$archive\" != \"$expected_archive\" ]]; then\n    printf 'unexpected chart archive: %s\\n' \"$archive\" >&2\n    exit 1\nfi\n\nchart_version=\"$(helm show chart \"$archive\" | yq eval '.version' -)\"\nif [[ \"$chart_version\" != \"${{ steps.version.outputs.version }}\" ]]; then\n    printf 'chart archive version does not match release version\\n' >&2\n    exit 1\nfi\n\nhelm template github-webhook-exporter \"$archive\" \\\n    --kube-version 1.35.0 >/dev/null\nprintf 'archive=%s\\n' \"$archive\" >> \"$GITHUB_OUTPUT\"\n",
    },
    {"uses": "docker/setup-buildx-action@8d2750c68a42422c14e847fe6c8ac0403b4cbd6f"},
    {
        "id": "metadata",
        "uses": "docker/metadata-action@c299e40c65443455700f0fdfc63efafe5b349051",
        "with": {
            "images": "ghcr.io/petergrace/github-webhook-exporter",
            "tags": "type=raw,value=${{ steps.version.outputs.version }}",
            "flavor": "latest=false",
            "labels": "org.opencontainers.image.created=${{ steps.version.outputs.commit_timestamp }}",
        },
    },
    {
        "uses": "docker/build-push-action@10e90e3645eae34f1e60eeb005ba3a3d33f178e8",
        "with": {
            "context": ".",
            "platforms": "linux/amd64",
            "load": True,
            "push": False,
            "provenance": False,
            "tags": "${{ steps.metadata.outputs.tags }}",
            "labels": "${{ steps.metadata.outputs.labels }}",
            "build-args": "SOURCE_DATE_EPOCH=${{ steps.version.outputs.source_date_epoch }}",
            "cache-from": "type=gha,scope=production-image-linux-amd64",
            "cache-to": "type=gha,mode=max,scope=production-image-linux-amd64",
        },
    },
    {
        "env": {
            "RELEASE_IMAGE": "ghcr.io/petergrace/github-webhook-exporter:${{ steps.version.outputs.version }}"
        },
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
        "env": {"GHCR_TOKEN": "${{ secrets.GITHUB_TOKEN }}"},
        "run": "printf '%s' \"$GHCR_TOKEN\" | helm registry login ghcr.io \\\n    --username \"${{ github.actor }}\" --password-stdin\n",
    },
    {
        "env": {
            "RELEASE_IMAGE": "ghcr.io/petergrace/github-webhook-exporter:${{ steps.version.outputs.version }}"
        },
        "run": "scripts/release-publish.sh \"${{ steps.version.outputs.version }}\" \\\n    \"$RELEASE_IMAGE\" \"${{ steps.chart.outputs.archive }}\"\n",
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

metadata_inputs = publish_steps[7].get("with", {})
metadata_tags = metadata_inputs.get("tags", "").lower()
if any(disallowed in metadata_tags for disallowed in ("latest", "branch", "sha")):
    fail("workflow publication metadata must use only the normalized release tag")

expected_created_label = (
    "org.opencontainers.image.created="
    "${{ steps.version.outputs.commit_timestamp }}"
)
if metadata_inputs.get("labels") != expected_created_label:
    fail("workflow image created label must use the checked-out commit timestamp")

version_step_run = publish_steps[1].get("run", "")
if "git show -s --format=%cI \"$GITHUB_SHA\"" not in version_step_run:
    fail("workflow image created label source must not use unconstrained current time")

for index, step in enumerate(publish_steps):
    if step.get("with", {}).get("push") is True:
        fail(f"workflow publication step {index + 1} must not use action-level push")
PY
