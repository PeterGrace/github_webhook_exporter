#!/usr/bin/env python3
"""Compare deterministic source and packaged-chart render matrices byte-for-byte."""

from __future__ import annotations

import pathlib
import sys


def fail(category: str) -> None:
    """Exit with a stable comparison category."""
    raise SystemExit(f"Helm render comparison failed: {category}")


def compare_matrices(source: pathlib.Path, archive: pathlib.Path, cases_file: pathlib.Path) -> None:
    """Require exactly the contracted files and byte-identical contents."""
    try:
        case_names = [line for line in cases_file.read_text(encoding="utf-8").splitlines() if line]
    except (OSError, UnicodeError):
        fail("RENDER001")
    expected_names = {f"{case_name}.yaml" for case_name in case_names}
    if len(expected_names) != len(case_names):
        fail("RENDER001")

    try:
        source_files = {path.name: path for path in source.glob("*.yaml") if path.is_file()}
        archive_files = {path.name: path for path in archive.glob("*.yaml") if path.is_file()}
    except OSError:
        fail("RENDER001")
    if set(source_files) != expected_names or set(archive_files) != expected_names:
        fail("RENDER002")

    try:
        for name in expected_names:
            if source_files[name].read_bytes() != archive_files[name].read_bytes():
                fail("RENDER003")
    except OSError:
        fail("RENDER003")


def main() -> None:
    """Parse paths and compare one source/archive render pair."""
    if len(sys.argv) != 4:
        raise SystemExit(
            f"usage: {pathlib.Path(sys.argv[0]).name} SOURCE_RENDERED ARCHIVE_RENDERED CASES_FILE"
        )
    compare_matrices(*(pathlib.Path(argument) for argument in sys.argv[1:]))


if __name__ == "__main__":
    main()
