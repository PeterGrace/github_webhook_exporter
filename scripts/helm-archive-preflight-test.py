#!/usr/bin/env python3
"""Regression tests for non-lossy Helm archive preflight validation."""

from __future__ import annotations

import importlib.util
import io
import pathlib
import sys
import tarfile
import tempfile

sys.dont_write_bytecode = True

MODULE_PATH = pathlib.Path(__file__).with_name("helm-archive-preflight.py")
SPEC = importlib.util.spec_from_file_location("helm_archive_preflight", MODULE_PATH)
if SPEC is None or SPEC.loader is None:
    raise SystemExit("unable to load archive preflight module")
PREFLIGHT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PREFLIGHT)


def write_archive(path: pathlib.Path, members: list[tuple[str, bytes, str]]) -> None:
    """Create an exact tar fixture without asking tar to normalize member names."""
    with tarfile.open(path, mode="w:gz") as archive:
        for name, contents, member_type in members:
            member = tarfile.TarInfo(name)
            member.size = len(contents)
            if member_type == "file":
                member.type = tarfile.REGTYPE
            elif member_type == "symlink":
                member.type = tarfile.SYMTYPE
                member.linkname = "/tmp/archive-target"
                member.size = 0
            elif member_type == "hardlink":
                member.type = tarfile.LNKTYPE
                member.linkname = "chart/Chart.yaml"
                member.size = 0
            elif member_type == "directory":
                member.type = tarfile.DIRTYPE
                member.size = 0
            else:
                raise AssertionError(f"unknown test member type: {member_type}")
            archive.addfile(member, io.BytesIO(contents) if member.size else None)


def expect_rejected(path: pathlib.Path, expected_category: str, sensitive_name: str) -> None:
    """Require rejection with a stable diagnostic that omits the member name."""
    try:
        PREFLIGHT.validate_archive(str(path), "chart")
    except SystemExit as error:
        diagnostic = str(error)
        assert diagnostic == f"Helm archive preflight failed: {expected_category}"
        assert sensitive_name not in diagnostic
    else:
        raise AssertionError("unsafe archive unexpectedly passed preflight")


def main() -> None:
    """Exercise whitespace, traversal, absolute-path, and link archive members."""
    with tempfile.TemporaryDirectory() as temporary_directory:
        root = pathlib.Path(temporary_directory)
        valid_archive = root / "valid.tgz"
        write_archive(
            valid_archive,
            [
                ("chart/Chart.yaml", b"apiVersion: v2\n", "file"),
                ("chart/file with whitespace", b"safe\n", "file"),
                ("chart/file\nwith-newline", b"safe\n", "file"),
            ],
        )
        PREFLIGHT.validate_archive(str(valid_archive), "chart")
        valid_extraction = root / "valid-extraction"
        PREFLIGHT.extract_archive(str(valid_archive), "chart", valid_extraction)
        assert (valid_extraction / "chart/file with whitespace").is_file()
        assert (valid_extraction / "chart/file\nwith-newline").is_file()

        attacks = [
            ("traversal.tgz", "chart/../../sensitive-traversal", "file", "ARCHIVE001"),
            ("absolute.tgz", "/sensitive-absolute", "file", "ARCHIVE001"),
            ("symlink.tgz", "chart/sensitive-symlink", "symlink", "ARCHIVE005"),
            ("hardlink.tgz", "chart/sensitive-hardlink", "hardlink", "ARCHIVE005"),
        ]
        collisions = [
            (
                "normalized-collision.tgz",
                [("chart/file", b"first", "file"), ("chart/./file", b"second", "file")],
            ),
            (
                "file-directory-alias.tgz",
                [("chart/alias/", b"", "directory"), ("chart/alias", b"data", "file")],
            ),
        ]
        for archive_name, members in collisions:
            archive_path = root / archive_name
            write_archive(archive_path, members)
            expect_rejected(archive_path, "ARCHIVE004", members[1][0])
            collision_destination = root / f"{archive_name}-extracted"
            collision_destination.mkdir()
            sentinel = collision_destination / "sentinel"
            sentinel.write_text("preserve-me\n", encoding="utf-8")
            try:
                PREFLIGHT.extract_archive(archive_path, "chart", collision_destination)
            except SystemExit as error:
                assert str(error) == "Helm archive preflight failed: ARCHIVE004"
                assert members[1][0] not in str(error)
            else:
                raise AssertionError("colliding archive unexpectedly reached extraction")
            assert list(collision_destination.iterdir()) == [sentinel]
            assert sentinel.read_text(encoding="utf-8") == "preserve-me\n"

        extraction_directory = root / "extracted"
        extraction_directory.mkdir()
        for archive_name, member_name, member_type, category in attacks:
            archive_path = root / archive_name
            write_archive(archive_path, [(member_name, b"", member_type)])
            expect_rejected(archive_path, category, member_name)
            try:
                PREFLIGHT.extract_archive(archive_path, "chart", extraction_directory)
            except SystemExit as error:
                assert str(error) == f"Helm archive preflight failed: {category}"
                assert member_name not in str(error)
            else:
                raise AssertionError("unsafe archive unexpectedly reached extraction")

        assert not any(extraction_directory.iterdir()), "rejected archive reached extraction"

    print("Archive preflight attack fixtures passed")


if __name__ == "__main__":
    main()
