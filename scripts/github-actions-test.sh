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

if [[ -z "${WORKFLOW_PATH}" ]]; then
    usage
fi

if [[ ! -f "${WORKFLOW_PATH}" ]]; then
    fail "missing workflow file: ${WORKFLOW_PATH}"
fi

python3 - "$WORKFLOW_PATH" <<'PY'
import sys
from pathlib import Path

import yaml

workflow_path = Path(sys.argv[1])
with workflow_path.open("r", encoding="utf-8") as handle:
    workflow = yaml.load(handle, Loader=yaml.BaseLoader)

if not isinstance(workflow, dict):
    raise SystemExit("workflow did not parse as a mapping")

on_section = workflow.get("on")
if not isinstance(on_section, dict):
    raise SystemExit("workflow must define pull_request and push triggers")

if "pull_request" not in on_section:
    raise SystemExit("workflow must trigger on pull_request")

push_section = on_section.get("push")
if not isinstance(push_section, dict):
    raise SystemExit("workflow must define a push trigger")

branches = push_section.get("branches")
if branches != ["main"]:
    raise SystemExit("workflow push trigger must target main only")

permissions = workflow.get("permissions")
if permissions != {"contents": "read"}:
    raise SystemExit("workflow must use least-privilege contents: read permissions")

jobs = workflow.get("jobs")
if not isinstance(jobs, dict) or len(jobs) != 1:
    raise SystemExit("workflow must define exactly one job")

_, job = next(iter(jobs.items()))
if not isinstance(job, dict):
    raise SystemExit("workflow job must be a mapping")

if job.get("runs-on") != "ubuntu-24.04":
    raise SystemExit("workflow must run on ubuntu-24.04")

concurrency = workflow.get("concurrency")
if not isinstance(concurrency, dict):
    raise SystemExit("workflow must define concurrency")

if concurrency.get("group") != "${{ github.workflow }}-${{ github.ref }}":
    raise SystemExit("workflow concurrency group must cancel stale branch runs")

if concurrency.get("cancel-in-progress") != "true":
    raise SystemExit("workflow concurrency must cancel in progress runs")

job_env = job.get("env")
if not isinstance(job_env, dict) or job_env.get("CONTAINER_IMAGE") != "github-webhook-exporter:ci":
    raise SystemExit("workflow must set CONTAINER_IMAGE=github-webhook-exporter:ci")

steps = job.get("steps")
if not isinstance(steps, list):
    raise SystemExit("workflow job must define steps")

allowed_uses = {
    f"actions/checkout@11d5960a326750d5838078e36cf38b85af677262",
    f"actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02",
}

for step in steps:
    if not isinstance(step, dict):
        raise SystemExit("workflow steps must be mappings")
    uses = step.get("uses")
    if uses is None:
        continue
    if uses not in allowed_uses:
        if "@" in uses:
            raise SystemExit(f"workflow uses mutable or unexpected action reference: {uses}")
        raise SystemExit(f"workflow uses an unexpected action: {uses}")

ordered_markers = [
    "scripts/install-ci-tools.sh",
    "shellcheck",
    "just helm-static",
    "just image-smoke",
    "just fmt",
    "cargo build --locked",
    "cargo clippy --all-targets -- -D warnings",
    "just test",
    "cargo doc --no-deps --locked",
]

step_index_by_marker: dict[str, int] = {}
for index, step in enumerate(steps):
    if not isinstance(step, dict):
        raise SystemExit("workflow steps must be mappings")
    run = step.get("run")
    if not isinstance(run, str):
        continue
    for marker in ordered_markers:
        if marker in run and marker not in step_index_by_marker:
            step_index_by_marker[marker] = index

missing = [marker for marker in ordered_markers if marker not in step_index_by_marker]
if missing:
    raise SystemExit(f"workflow is missing required commands: {', '.join(missing)}")

indices = [step_index_by_marker[marker] for marker in ordered_markers]
if indices != sorted(indices):
    raise SystemExit("workflow commands are not in the required order")

upload_step = None
for step in steps:
    if isinstance(step, dict) and step.get("uses") == "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02":
        upload_step = step
        break

if upload_step is None:
    raise SystemExit("workflow must upload the packaged archive")

with_section = upload_step.get("with")
if not isinstance(with_section, dict):
    raise SystemExit("upload-artifact step must define with:")

if with_section.get("path") != "dist/github-webhook-exporter-0.1.0.tgz":
    raise SystemExit("workflow must upload the exact packaged archive path")

if with_section.get("if-no-files-found") != "error":
    raise SystemExit("workflow must fail when the packaged archive is missing")

if with_section.get("retention-days") != "30":
    raise SystemExit("workflow must set a fixed artifact retention period")
PY
