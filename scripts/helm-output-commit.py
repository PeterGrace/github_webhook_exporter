#!/usr/bin/env python3
"""Atomically replace a generated Helm output directory on Linux."""

from __future__ import annotations

import ctypes
import errno
import fcntl
import os
import stat
import sys
from dataclasses import dataclass

RENAME_NOREPLACE = 1
RENAME_EXCHANGE = 2
GENERATED_MARKER = ".gwe-generated-output"
ROOT_MARKER = ".gwe-generated-root"
ROOT_MARKER_VALUE = "gwe-generated-root"
OPEN_DIRECTORY_FLAGS = os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC | os.O_NOFOLLOW


class UnsafeOutput(Exception):
    """Indicate that output identities or atomic replacement guarantees failed."""


@dataclass(frozen=True)
class Identity:
    """Identify one filesystem object without relying on its path spelling."""

    device: int
    inode: int

    @classmethod
    def from_stat(cls, status: os.stat_result) -> Identity:
        """Build an identity from an operating-system stat result."""
        return cls(status.st_dev, status.st_ino)


@dataclass(frozen=True)
class Snapshot:
    """Record identities approved after output preparation."""

    parent: Identity
    destination: Identity | None
    stage: Identity

    def serialize(self) -> str:
        """Return a numeric shell-safe representation of this snapshot."""
        destination = self.destination or Identity(0, 0)
        return " ".join(
            str(value)
            for value in (
                self.parent.device,
                self.parent.inode,
                int(self.destination is not None),
                destination.device,
                destination.inode,
                self.stage.device,
                self.stage.inode,
            )
        )


def parse_identity(device: str, inode: str) -> Identity:
    """Parse an identity supplied by the trusted shell wrapper."""
    identity = Identity(int(device), int(inode))
    if identity.device <= 0 or identity.inode <= 0:
        raise UnsafeOutput
    return identity


def validate_name(name: str) -> None:
    """Require a single non-special directory entry name."""
    if not name or name in {".", ".."} or "/" in name or "\x00" in name:
        raise UnsafeOutput


def open_parent(parent_path: str, expected: Identity | None = None) -> int:
    """Open and lock a non-symlink parent directory, optionally checking its identity."""
    parent_fd = os.open(parent_path, OPEN_DIRECTORY_FLAGS)
    try:
        fcntl.flock(parent_fd, fcntl.LOCK_EX)
        status = os.fstat(parent_fd)
        if expected is not None and Identity.from_stat(status) != expected:
            raise UnsafeOutput
    except Exception:
        os.close(parent_fd)
        raise
    return parent_fd


def entry_status(parent_fd: int, name: str) -> os.stat_result | None:
    """Return no-follow status for a child, or None when it is absent."""
    try:
        return os.stat(name, dir_fd=parent_fd, follow_symlinks=False)
    except FileNotFoundError:
        return None


def open_directory(parent_fd: int, name: str, expected: Identity | None = None) -> int:
    """Open a child directory without following a symlink and verify its identity."""
    directory_fd = os.open(name, OPEN_DIRECTORY_FLAGS, dir_fd=parent_fd)
    try:
        status = os.fstat(directory_fd)
        if expected is not None and Identity.from_stat(status) != expected:
            raise UnsafeOutput
    except Exception:
        os.close(directory_fd)
        raise
    return directory_fd


def marker_matches(directory_fd: int, marker_name: str, expected_value: str) -> bool:
    """Return whether a no-follow regular marker has the exact expected contents."""
    try:
        marker_fd = os.open(
            marker_name,
            os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW,
            dir_fd=directory_fd,
        )
    except OSError:
        return False
    try:
        marker_status = os.fstat(marker_fd)
        if not stat.S_ISREG(marker_status.st_mode) or marker_status.st_size > 128:
            return False
        return os.read(marker_fd, 129) == f"{expected_value}\n".encode()
    except OSError:
        return False
    finally:
        os.close(marker_fd)


def validate_owned_directory(
    parent_fd: int,
    name: str,
    expected_identity: Identity,
    output_kind: str,
    allow_generated_root: bool,
) -> None:
    """Require an unchanged directory carrying an approved ownership marker."""
    directory_fd = open_directory(parent_fd, name, expected_identity)
    try:
        owned = marker_matches(directory_fd, GENERATED_MARKER, output_kind)
        if allow_generated_root:
            owned = owned or marker_matches(directory_fd, ROOT_MARKER, ROOT_MARKER_VALUE)
        if not owned:
            raise UnsafeOutput
    finally:
        os.close(directory_fd)


def snapshot(
    parent_path: str,
    destination_name: str,
    stage_name: str,
    output_kind: str,
    allow_generated_root: bool,
) -> Snapshot:
    """Capture parent, destination, and stage identities after validating ownership."""
    validate_name(destination_name)
    validate_name(stage_name)
    parent_fd = open_parent(parent_path)
    try:
        parent_identity = Identity.from_stat(os.fstat(parent_fd))
        stage_status = entry_status(parent_fd, stage_name)
        if stage_status is None or not stat.S_ISDIR(stage_status.st_mode):
            raise UnsafeOutput
        stage_identity = Identity.from_stat(stage_status)
        validate_owned_directory(parent_fd, stage_name, stage_identity, output_kind, False)

        destination_status = entry_status(parent_fd, destination_name)
        if destination_status is None:
            destination_identity = None
        else:
            if not stat.S_ISDIR(destination_status.st_mode):
                raise UnsafeOutput
            destination_identity = Identity.from_stat(destination_status)
            validate_owned_directory(
                parent_fd,
                destination_name,
                destination_identity,
                output_kind,
                allow_generated_root,
            )
        return Snapshot(parent_identity, destination_identity, stage_identity)
    finally:
        os.close(parent_fd)


def renameat2(
    parent_fd: int, source_name: str, destination_name: str, flags: int
) -> None:
    """Invoke Linux renameat2 with directory-relative names and required semantics."""
    try:
        function = ctypes.CDLL(None, use_errno=True).renameat2
    except AttributeError as error:
        raise UnsafeOutput from error
    function.argtypes = [
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_int,
        ctypes.c_char_p,
        ctypes.c_uint,
    ]
    function.restype = ctypes.c_int
    result = function(
        parent_fd,
        os.fsencode(source_name),
        parent_fd,
        os.fsencode(destination_name),
        flags,
    )
    if result != 0:
        error_number = ctypes.get_errno()
        if error_number in {errno.ENOSYS, errno.EINVAL, errno.EOPNOTSUPP, errno.EXDEV}:
            raise UnsafeOutput
        raise OSError(error_number, os.strerror(error_number))


def remove_directory_tree(
    parent_fd: int, name: str, expected: Identity, expected_device: int
) -> None:
    """Delete only an identity-checked, same-filesystem directory tree without following links."""
    directory_fd = open_directory(parent_fd, name, expected)
    try:
        for child_name in os.listdir(directory_fd):
            child_status = os.stat(child_name, dir_fd=directory_fd, follow_symlinks=False)
            child_identity = Identity.from_stat(child_status)
            if stat.S_ISDIR(child_status.st_mode):
                if child_identity.device != expected_device:
                    raise UnsafeOutput
                remove_directory_tree(
                    directory_fd, child_name, child_identity, expected_device
                )
            else:
                os.unlink(child_name, dir_fd=directory_fd)
        current = entry_status(parent_fd, name)
        if current is None or Identity.from_stat(current) != expected:
            raise UnsafeOutput
        os.rmdir(name, dir_fd=parent_fd)
    finally:
        os.close(directory_fd)


def commit(
    parent_path: str,
    destination_name: str,
    stage_name: str,
    output_kind: str,
    allow_generated_root: bool,
    approved: Snapshot,
) -> None:
    """Revalidate identities, atomically commit the stage, and remove only swapped output."""
    validate_name(destination_name)
    validate_name(stage_name)
    parent_fd = open_parent(parent_path, approved.parent)
    try:
        stage_status = entry_status(parent_fd, stage_name)
        if (
            stage_status is None
            or not stat.S_ISDIR(stage_status.st_mode)
            or Identity.from_stat(stage_status) != approved.stage
        ):
            raise UnsafeOutput
        validate_owned_directory(parent_fd, stage_name, approved.stage, output_kind, False)

        destination_status = entry_status(parent_fd, destination_name)
        if approved.destination is None:
            if destination_status is not None:
                raise UnsafeOutput
            renameat2(parent_fd, stage_name, destination_name, RENAME_NOREPLACE)
            committed_status = entry_status(parent_fd, destination_name)
            if committed_status is None or Identity.from_stat(committed_status) != approved.stage:
                raise UnsafeOutput
            return

        if (
            destination_status is None
            or not stat.S_ISDIR(destination_status.st_mode)
            or Identity.from_stat(destination_status) != approved.destination
        ):
            raise UnsafeOutput
        validate_owned_directory(
            parent_fd,
            destination_name,
            approved.destination,
            output_kind,
            allow_generated_root,
        )
        renameat2(parent_fd, stage_name, destination_name, RENAME_EXCHANGE)
        committed_status = entry_status(parent_fd, destination_name)
        swapped_status = entry_status(parent_fd, stage_name)
        if (
            committed_status is None
            or Identity.from_stat(committed_status) != approved.stage
            or swapped_status is None
            or Identity.from_stat(swapped_status) != approved.destination
        ):
            try:
                renameat2(parent_fd, stage_name, destination_name, RENAME_EXCHANGE)
            except (OSError, UnsafeOutput):
                pass
            raise UnsafeOutput
        remove_directory_tree(
            parent_fd,
            stage_name,
            approved.destination,
            approved.destination.device,
        )
    finally:
        os.close(parent_fd)


def cleanup(parent_path: str, stage_name: str, approved: Snapshot) -> None:
    """Remove only the unchanged prepared stage after a failed or interrupted generation."""
    validate_name(stage_name)
    parent_fd = open_parent(parent_path, approved.parent)
    try:
        stage_status = entry_status(parent_fd, stage_name)
        if stage_status is None:
            return
        if Identity.from_stat(stage_status) != approved.stage:
            raise UnsafeOutput
        remove_directory_tree(
            parent_fd, stage_name, approved.stage, approved.stage.device
        )
    finally:
        os.close(parent_fd)


def parse_boolean(value: str) -> bool:
    """Parse a shell boolean while rejecting ambiguous values."""
    if value not in {"0", "1"}:
        raise UnsafeOutput
    return value == "1"


def parse_snapshot(arguments: list[str]) -> Snapshot:
    """Parse the seven numeric fields emitted by snapshot."""
    if len(arguments) != 7 or arguments[2] not in {"0", "1"}:
        raise UnsafeOutput
    parent = parse_identity(arguments[0], arguments[1])
    destination = (
        parse_identity(arguments[3], arguments[4]) if arguments[2] == "1" else None
    )
    stage = parse_identity(arguments[5], arguments[6])
    return Snapshot(parent, destination, stage)


def main() -> None:
    """Dispatch snapshot, commit, or cleanup without disclosing caller-controlled values."""
    try:
        command = sys.argv[1]
        if command == "snapshot" and len(sys.argv) == 7:
            approved = snapshot(
                sys.argv[2],
                sys.argv[3],
                sys.argv[4],
                sys.argv[5],
                parse_boolean(sys.argv[6]),
            )
            print(approved.serialize())
            return
        if command == "commit" and len(sys.argv) == 14:
            commit(
                sys.argv[2],
                sys.argv[3],
                sys.argv[4],
                sys.argv[5],
                parse_boolean(sys.argv[6]),
                parse_snapshot(sys.argv[7:]),
            )
            return
        if command == "cleanup" and len(sys.argv) == 11:
            cleanup(sys.argv[2], sys.argv[3], parse_snapshot(sys.argv[4:]))
            return
        raise UnsafeOutput
    except (IndexError, OSError, OverflowError, UnsafeOutput, ValueError):
        raise SystemExit(1) from None


if __name__ == "__main__":
    main()
