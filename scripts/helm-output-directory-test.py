#!/usr/bin/env python3
"""Regression tests for atomic, identity-bound Helm output replacement."""

from __future__ import annotations

import pathlib
import subprocess
import tempfile

OUTPUT_GUARD = pathlib.Path(__file__).with_name("helm-output-directory.sh").resolve()
EXPECTED_DIAGNOSTIC = "Test output failed: unsafe output directory\n"


def run_bash(
    script: str, *arguments: pathlib.Path, expect_success: bool
) -> subprocess.CompletedProcess[str]:
    """Run a guard scenario and require its expected success state."""
    result = subprocess.run(
        ["bash", "-c", script, "output-test", OUTPUT_GUARD, *map(str, arguments)],
        capture_output=True,
        text=True,
    )
    assert (result.returncode == 0) is expect_success, result.stderr
    return result


def generate(
    repository: pathlib.Path, chart: pathlib.Path, output: pathlib.Path, payload: str
) -> None:
    """Generate and atomically commit one output payload through the public shell interface."""
    result = run_bash(
        r'''
set -Eeuo pipefail
source "$1"
export HELM_OUTPUT_ERROR_PREFIX="Test output"
helm_output_prepare "test-output" "$3" "$4" "$2"
trap helm_output_cleanup_stage EXIT
printf '%s\n' "$5" >"${HELM_OUTPUT_STAGE}/payload"
helm_output_commit
''',
        repository,
        chart,
        output,
        pathlib.Path(payload),
        expect_success=True,
    )
    assert result.stderr == ""


def assert_identity_substitution_rejected(
    repository: pathlib.Path, chart: pathlib.Path, output: pathlib.Path
) -> None:
    """Require commit to reject replacement of an approved destination inode."""
    preserved = output.with_name("approved-output")
    result = run_bash(
        r'''
set -Eeuo pipefail
source "$1"
export HELM_OUTPUT_ERROR_PREFIX="Test output"
helm_output_prepare "test-output" "$3" "$4" "$2"
trap helm_output_cleanup_stage EXIT
printf '%s\n' staged >"${HELM_OUTPUT_STAGE}/payload"
mv -- "$4" "$5"
mkdir -- "$4"
printf '%s\n' test-output >"$4/.gwe-generated-output"
printf '%s\n' preserve-me >"$4/sentinel"
helm_output_commit
''',
        repository,
        chart,
        output,
        preserved,
        expect_success=False,
    )
    assert result.stderr == EXPECTED_DIAGNOSTIC
    assert (output / "sentinel").read_text(encoding="utf-8") == "preserve-me\n"
    assert (preserved / "payload").is_file()
    assert not list(output.parent.glob(".generated.stage.*"))


def assert_parent_substitution_rejected(
    repository: pathlib.Path, chart: pathlib.Path, output: pathlib.Path
) -> None:
    """Require commit to reject replacement of the approved parent inode."""
    parent = output.parent
    preserved_parent = parent.with_name("approved-parent")
    result = run_bash(
        r'''
set -Eeuo pipefail
source "$1"
export HELM_OUTPUT_ERROR_PREFIX="Test output"
helm_output_prepare "test-output" "$3" "$4" "$2"
trap helm_output_cleanup_stage EXIT
stage_name="${HELM_OUTPUT_STAGE##*/}"
mv -- "${4%/*}" "$5"
mkdir -- "${4%/*}"
mkdir -- "${4%/*}/${stage_name}"
printf '%s\n' test-output >"${4%/*}/${stage_name}/.gwe-generated-output"
printf '%s\n' preserve-me >"${4%/*}/${stage_name}/sentinel"
mkdir -- "$4"
printf '%s\n' test-output >"$4/.gwe-generated-output"
helm_output_commit
''',
        repository,
        chart,
        output,
        preserved_parent,
        expect_success=False,
    )
    assert result.stderr == EXPECTED_DIAGNOSTIC
    assert (parent / output.name).is_dir()
    assert (preserved_parent / output.name / "payload").is_file()


def main() -> None:
    """Exercise repeat replacement and commit-time identity substitutions."""
    with tempfile.TemporaryDirectory() as temporary_directory:
        root = pathlib.Path(temporary_directory)
        repository = root / "repository"
        chart = repository / "chart"
        chart.mkdir(parents=True)

        output = root / "outputs" / "generated"
        output.parent.mkdir()
        generate(repository, chart, output, "first")
        generate(repository, chart, output, "second")
        assert (output / "payload").read_text(encoding="utf-8") == "second\n"
        assert not list(output.parent.glob(".generated.stage.*"))

        assert_identity_substitution_rejected(repository, chart, output)

        parent_output = root / "parent-outputs" / "generated"
        parent_output.parent.mkdir()
        generate(repository, chart, parent_output, "original")
        assert_parent_substitution_rejected(repository, chart, parent_output)

    print("Output-directory atomic replacement fixtures passed")


if __name__ == "__main__":
    main()
