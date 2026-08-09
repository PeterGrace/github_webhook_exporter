#!/usr/bin/env python3
"""Validate Helm archive members without disclosing archive member names."""

from __future__ import annotations

import pathlib
import sys
import tarfile

FORBIDDEN_PARTS = {
    ".superpowers",
    "changelog",
    "ci",
    "dist",
    "scripts",
    "target",
    "tests",
}


def fail(category: str) -> None:
    """Exit with a stable, value-free archive diagnostic."""
    raise SystemExit(f"Helm archive preflight failed: {category}")


def validate_member_name(name: str, archive_root: str) -> tuple[str, ...]:
    """Validate and return the normalized POSIX path components for one member."""
    if not name or name.startswith("/") or "//" in name or "\\" in name:
        fail("ARCHIVE001")
    if any(ord(character) == 0 for character in name):
        fail("ARCHIVE001")

    stripped_name = name[:-1] if name.endswith("/") else name
    path = pathlib.PurePosixPath(stripped_name)
    parts = path.parts
    if not parts or any(part in {"", ".", ".."} for part in parts):
        fail("ARCHIVE001")
    if parts[0] != archive_root:
        fail("ARCHIVE002")
    if any(part in FORBIDDEN_PARTS or "negative" in part.lower() for part in parts[1:]):
        fail("ARCHIVE003")
    return parts


def validated_members(archive: tarfile.TarFile, archive_root: str) -> list[tarfile.TarInfo]:
    """Validate and return all members from an already-open archive."""
    if not archive_root or "/" in archive_root or archive_root in {".", ".."}:
        fail("ARCHIVE002")

    members = archive.getmembers()
    seen_targets: set[tuple[str, ...]] = set()
    for member in members:
        extraction_target = validate_member_name(member.name, archive_root)
        if extraction_target in seen_targets:
            fail("ARCHIVE004")
        seen_targets.add(extraction_target)
        if not (member.isfile() or member.isdir()):
            fail("ARCHIVE005")
    if not members:
        fail("ARCHIVE006")
    return members


def validate_archive(archive_path: str, archive_root: str) -> None:
    """Validate every archive member without extracting it."""
    try:
        with tarfile.open(archive_path, mode="r:gz") as archive:
            validated_members(archive, archive_root)
    except (OSError, tarfile.TarError, UnicodeError):
        fail("ARCHIVE006")


def extract_archive(archive_path: str, archive_root: str, destination: pathlib.Path) -> None:
    """Validate the complete archive, then extract only regular files and directories."""
    try:
        with tarfile.open(archive_path, mode="r:gz") as archive:
            members = validated_members(archive, archive_root)
            destination.mkdir(parents=True, exist_ok=True)
            for member in members:
                target = destination.joinpath(*pathlib.PurePosixPath(member.name).parts)
                if member.isdir():
                    target.mkdir(parents=True, exist_ok=True)
                    continue
                target.parent.mkdir(parents=True, exist_ok=True)
                source = archive.extractfile(member)
                if source is None:
                    fail("ARCHIVE006")
                with target.open("xb") as target_file:
                    target_file.write(source.read())
    except (OSError, tarfile.TarError, UnicodeError):
        fail("ARCHIVE006")


def main() -> None:
    """Parse command-line arguments and validate or safely extract one archive."""
    if len(sys.argv) not in {3, 4}:
        raise SystemExit(
            f"usage: {pathlib.Path(sys.argv[0]).name} ARCHIVE ROOT [DESTINATION]"
        )
    if len(sys.argv) == 4:
        extract_archive(sys.argv[1], sys.argv[2], pathlib.Path(sys.argv[3]))
    else:
        validate_archive(sys.argv[1], sys.argv[2])


if __name__ == "__main__":
    main()
