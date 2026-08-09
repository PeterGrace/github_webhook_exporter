#!/usr/bin/env python3
"""Regression tests for atomic, identity-bound Helm output replacement."""

from __future__ import annotations

import importlib.util
import os
import pathlib
import subprocess
import sys
import tempfile
from unittest import mock

OUTPUT_GUARD = pathlib.Path(__file__).with_name("helm-output-directory.sh").resolve()
COMMIT_HELPER = pathlib.Path(__file__).with_name("helm-output-commit.py").resolve()
EXPECTED_DIAGNOSTIC = "Test output failed: unsafe output directory\n"
EXPECTED_CLEANUP_WARNING = "Test output warning: committed output cleanup deferred\n"

sys.dont_write_bytecode = True
SPEC = importlib.util.spec_from_file_location("helm_output_commit", COMMIT_HELPER)
if SPEC is None or SPEC.loader is None:
    raise SystemExit("unable to load output commit helper")
COMMIT = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = COMMIT
SPEC.loader.exec_module(COMMIT)


def run_bash(
    script: str,
    *arguments: pathlib.Path | str,
    expect_success: bool,
    guard: pathlib.Path = OUTPUT_GUARD,
    environment: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
    """Run a guard scenario and require its expected success state."""
    command_environment = os.environ.copy()
    if environment is not None:
        command_environment.update(environment)
    result = subprocess.run(
        ["bash", "-c", script, "output-test", str(guard), *map(str, arguments)],
        capture_output=True,
        text=True,
        env=command_environment,
    )
    assert (result.returncode == 0) is expect_success, result.stderr
    return result


def generate(
    repository: pathlib.Path,
    chart: pathlib.Path,
    output: pathlib.Path,
    payload: str,
    guard: pathlib.Path = OUTPUT_GUARD,
) -> None:
    """Generate and atomically commit one payload through the public shell interface."""
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
        payload,
        expect_success=True,
        guard=guard,
    )
    assert result.stderr == ""


def make_fault_guard(root: pathlib.Path) -> pathlib.Path:
    """Create a test-only helper and guard copy with deterministic fault injection."""
    fault_helper = root / "fault-helper.py"
    fault_helper.write_text(
        f'''#!/usr/bin/env python3
import importlib.util
import os
import pathlib
import subprocess
import sys
from unittest import mock

sys.dont_write_bytecode = True
REAL_HELPER = pathlib.Path({str(COMMIT_HELPER)!r})
mode = os.environ.get("GWE_OUTPUT_TEST_FAULT", "")
command = sys.argv[1] if len(sys.argv) > 1 else ""
if mode == "malformed-prepare" and command == "prepare":
    result = subprocess.run(
        [sys.executable, str(REAL_HELPER), *sys.argv[1:]],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise SystemExit(result.returncode)
    fields = result.stdout.split()
    fields[3] = "invalid"
    print(" ".join(fields))
    raise SystemExit(0)
if mode == "cleanup-once" and command == "cleanup-committed":
    state = pathlib.Path(os.environ["GWE_OUTPUT_TEST_STATE"])
    if not state.exists():
        state.write_text("failed\\n", encoding="utf-8")
        raise SystemExit(1)
if mode == "interrupt-exchange-once" and command == "commit":
    state = pathlib.Path(os.environ["GWE_OUTPUT_TEST_STATE"])
    if not state.exists():
        state.write_text("interrupted\\n", encoding="utf-8")
        spec = importlib.util.spec_from_file_location("interrupted_helm_output_commit", REAL_HELPER)
        module = importlib.util.module_from_spec(spec)
        sys.modules[spec.name] = module
        spec.loader.exec_module(module)
        real_renameat2 = module.renameat2

        def exchange_then_interrupt(parent_fd, source_name, destination_name, flags):
            real_renameat2(parent_fd, source_name, destination_name, flags)
            raise module.UnsafeOutput

        with mock.patch.object(module, "renameat2", side_effect=exchange_then_interrupt):
            module.main()
if mode == "unsupported" and command == "commit":
    spec = importlib.util.spec_from_file_location("faulted_helm_output_commit", REAL_HELPER)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    with mock.patch.object(module, "renameat2", side_effect=module.UnsafeOutput):
        module.main()
os.execv(sys.executable, [sys.executable, str(REAL_HELPER), *sys.argv[1:]])
''',
        encoding="utf-8",
    )
    fault_helper.chmod(0o700)

    guard = root / "helm-output-directory.sh"
    guard_text = OUTPUT_GUARD.read_text(encoding="utf-8")
    helper_assignment = (
        'readonly HELM_OUTPUT_COMMIT_HELPER="${HELM_OUTPUT_GUARD_DIRECTORY}/'
        'helm-output-commit.py"'
    )
    replacement = f'readonly HELM_OUTPUT_COMMIT_HELPER="{fault_helper}"'
    assert helper_assignment in guard_text
    guard.write_text(guard_text.replace(helper_assignment, replacement), encoding="utf-8")
    return guard


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
    """Require commit and cleanup to reject replacement of the approved parent inode."""
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
    decoy_stages = list(parent.glob(f".{output.name}.stage.*"))
    assert len(decoy_stages) == 1
    assert (decoy_stages[0] / "sentinel").read_text(encoding="utf-8") == "preserve-me\n"
    assert (preserved_parent / output.name / "payload").is_file()


def assert_prepare_parse_failure_cleans_stage(
    repository: pathlib.Path,
    chart: pathlib.Path,
    output: pathlib.Path,
    fault_guard: pathlib.Path,
) -> None:
    """Require malformed snapshot output to trigger identity-safe stage cleanup."""
    result = run_bash(
        r'''
set -Eeuo pipefail
source "$1"
export HELM_OUTPUT_ERROR_PREFIX="Test output"
if helm_output_prepare "test-output" "$3" "$4" "$2"; then
    exit 9
fi
[[ -z "${HELM_OUTPUT_STAGE:-}" ]]
[[ ${#HELM_OUTPUT_SNAPSHOT[@]} -eq 0 ]]
[[ -z "$(find "${4%/*}" -maxdepth 1 -name ".${4##*/}.stage.*" -print -quit)" ]]
''',
        repository,
        chart,
        output,
        expect_success=True,
        guard=fault_guard,
        environment={"GWE_OUTPUT_TEST_FAULT": "malformed-prepare"},
    )
    assert result.stderr == EXPECTED_DIAGNOSTIC


def assert_unsupported_primitive_fails_closed(
    repository: pathlib.Path,
    chart: pathlib.Path,
    output: pathlib.Path,
    fault_guard: pathlib.Path,
) -> None:
    """Require simulated renameat2 unavailability to preserve both approved directories."""
    stage_record = output.parent / "stage-record"
    result = run_bash(
        r'''
set -Eeuo pipefail
source "$1"
export HELM_OUTPUT_ERROR_PREFIX="Test output"
helm_output_prepare "test-output" "$3" "$4" "$2"
printf '%s\n' staged >"${HELM_OUTPUT_STAGE}/payload"
stage_name="${HELM_OUTPUT_STAGE##*/}"
stage_inode="$(stat -c '%d:%i' -- "${HELM_OUTPUT_STAGE}")"
destination_inode="$(stat -c '%d:%i' -- "$4")"
if helm_output_commit; then
    exit 9
fi
[[ "${HELM_OUTPUT_COMMITTED}" == 0 ]]
[[ "$(stat -c '%d:%i' -- "${HELM_OUTPUT_STAGE}")" == "${stage_inode}" ]]
[[ "$(stat -c '%d:%i' -- "$4")" == "${destination_inode}" ]]
[[ "$(<"${HELM_OUTPUT_STAGE}/payload")" == staged ]]
[[ "$(<"$4/payload")" == original ]]
printf '%s\n' "${stage_name}" >"$5"
''',
        repository,
        chart,
        output,
        stage_record,
        expect_success=True,
        guard=fault_guard,
        environment={"GWE_OUTPUT_TEST_FAULT": "unsupported"},
    )
    assert result.stderr == EXPECTED_DIAGNOSTIC
    stage_name = stage_record.read_text(encoding="utf-8").strip()
    assert (output.parent / stage_name / "payload").read_text(encoding="utf-8") == "staged\n"


def assert_interrupted_shell_commit_recovers(
    repository: pathlib.Path,
    chart: pathlib.Path,
    output: pathlib.Path,
    fault_guard: pathlib.Path,
    fault_state: pathlib.Path,
) -> None:
    """Require the shell to retry exact inode placement after helper interruption."""
    result = run_bash(
        r'''
set -Eeuo pipefail
source "$1"
export HELM_OUTPUT_ERROR_PREFIX="Test output"
helm_output_prepare "test-output" "$3" "$4" "$2"
printf '%s\n' replacement >"${HELM_OUTPUT_STAGE}/payload"
helm_output_commit
[[ "${HELM_OUTPUT_COMMITTED}" == 1 ]]
[[ -z "${HELM_OUTPUT_STAGE}" ]]
[[ ${#HELM_OUTPUT_SNAPSHOT[@]} -eq 0 ]]
[[ "$(<"$4/payload")" == replacement ]]
''',
        repository,
        chart,
        output,
        expect_success=True,
        guard=fault_guard,
        environment={
            "GWE_OUTPUT_TEST_FAULT": "interrupt-exchange-once",
            "GWE_OUTPUT_TEST_STATE": str(fault_state),
        },
    )
    assert result.stderr == ""
    assert fault_state.read_text(encoding="utf-8") == "interrupted\n"


def assert_cleanup_failure_is_committed_and_retryable(
    repository: pathlib.Path,
    chart: pathlib.Path,
    output: pathlib.Path,
    fault_guard: pathlib.Path,
    fault_state: pathlib.Path,
) -> None:
    """Require cleanup failure to warn after commit and permit identity-safe retry."""
    result = run_bash(
        r'''
set -Eeuo pipefail
source "$1"
export HELM_OUTPUT_ERROR_PREFIX="Test output"
helm_output_prepare "test-output" "$3" "$4" "$2"
printf '%s\n' replacement >"${HELM_OUTPUT_STAGE}/payload"
stage_path="${HELM_OUTPUT_STAGE}"
helm_output_commit
[[ "${HELM_OUTPUT_COMMITTED}" == 1 ]]
[[ "${HELM_OUTPUT_STAGE}" == "${stage_path}" ]]
[[ "$(<"$4/payload")" == replacement ]]
[[ "$(<"${stage_path}/payload")" == original ]]

preserved_old="${stage_path}.preserved"
mv -- "${stage_path}" "${preserved_old}"
mkdir -- "${stage_path}"
printf '%s\n' decoy >"${stage_path}/sentinel"
helm_output_cleanup_stage
[[ -f "${stage_path}/sentinel" ]]
[[ -n "${HELM_OUTPUT_STAGE}" ]]

rm -- "${stage_path}/sentinel"
rmdir -- "${stage_path}"
mv -- "${preserved_old}" "${stage_path}"
helm_output_cleanup_stage
[[ -z "${HELM_OUTPUT_STAGE}" ]]
[[ ${#HELM_OUTPUT_SNAPSHOT[@]} -eq 0 ]]
[[ ! -e "${stage_path}" ]]
[[ "$(<"$4/payload")" == replacement ]]
''',
        repository,
        chart,
        output,
        expect_success=True,
        guard=fault_guard,
        environment={
            "GWE_OUTPUT_TEST_FAULT": "cleanup-once",
            "GWE_OUTPUT_TEST_STATE": str(fault_state),
        },
    )
    assert result.stderr == EXPECTED_CLEANUP_WARNING


def assert_interrupted_exchange_recovers(root: pathlib.Path) -> None:
    """Require retry to recognize an exchange interrupted immediately after commit."""
    parent = root / "interrupted"
    destination = parent / "generated"
    stage = parent / ".generated.stage.injected"
    destination.mkdir(parents=True)
    stage.mkdir()
    (destination / ".gwe-generated-output").write_text("test-output\n", encoding="utf-8")
    (destination / "payload").write_text("old\n", encoding="utf-8")
    (stage / ".gwe-generated-output").write_text("test-output\n", encoding="utf-8")
    (stage / "payload").write_text("new\n", encoding="utf-8")
    approved = COMMIT.snapshot(str(parent), destination.name, stage.name, "test-output", False)
    real_renameat2 = COMMIT.renameat2

    def exchange_then_interrupt(
        parent_fd: int, source_name: str, destination_name: str, flags: int
    ) -> None:
        real_renameat2(parent_fd, source_name, destination_name, flags)
        raise RuntimeError("injected interruption")

    try:
        with mock.patch.object(COMMIT, "renameat2", side_effect=exchange_then_interrupt):
            COMMIT.commit(
                str(parent), destination.name, stage.name, "test-output", False, approved
            )
    except RuntimeError as error:
        assert str(error) == "injected interruption"
    else:
        raise AssertionError("injected post-exchange interruption did not occur")

    assert (destination / "payload").read_text(encoding="utf-8") == "new\n"
    assert (stage / "payload").read_text(encoding="utf-8") == "old\n"
    COMMIT.commit(str(parent), destination.name, stage.name, "test-output", False, approved)
    COMMIT.cleanup_committed(str(parent), stage.name, approved)
    assert not stage.exists()
    assert (destination / "payload").read_text(encoding="utf-8") == "new\n"


def main() -> None:
    """Exercise replacement, deterministic faults, and commit-time identity attacks."""
    with tempfile.TemporaryDirectory(prefix="gwe-output-test-", dir="/tmp") as temporary_directory:
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

        repository_output = repository / "dist" / "rendered"
        generate(repository, chart, repository_output, "nested-first")
        generate(repository, chart, repository_output, "nested-second")
        assert (repository_output / "payload").read_text(encoding="utf-8") == "nested-second\n"
        generate(repository, chart, repository / "dist", "dist-first")
        generate(repository, chart, repository / "dist", "dist-second")
        assert (repository / "dist/payload").read_text(encoding="utf-8") == "dist-second\n"

        fault_root = root / "faults"
        fault_root.mkdir()
        fault_guard = make_fault_guard(fault_root)
        malformed_output = root / "malformed" / "generated"
        malformed_output.parent.mkdir()
        assert_prepare_parse_failure_cleans_stage(
            repository, chart, malformed_output, fault_guard
        )

        unsupported_output = root / "unsupported" / "generated"
        unsupported_output.parent.mkdir()
        generate(repository, chart, unsupported_output, "original")
        assert_unsupported_primitive_fails_closed(
            repository, chart, unsupported_output, fault_guard
        )

        interrupted_output = root / "shell-interrupted" / "generated"
        interrupted_output.parent.mkdir()
        generate(repository, chart, interrupted_output, "original")
        assert_interrupted_shell_commit_recovers(
            repository,
            chart,
            interrupted_output,
            fault_guard,
            fault_root / "interruption-state",
        )

        cleanup_output = root / "cleanup" / "generated"
        cleanup_output.parent.mkdir()
        generate(repository, chart, cleanup_output, "original")
        assert_cleanup_failure_is_committed_and_retryable(
            repository,
            chart,
            cleanup_output,
            fault_guard,
            fault_root / "cleanup-state",
        )
        assert_interrupted_exchange_recovers(root)

    print("Output-directory atomic replacement fixtures passed")


if __name__ == "__main__":
    main()
