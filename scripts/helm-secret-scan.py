#!/usr/bin/env python3
"""Structurally scan YAML/JSON and text files for embedded credentials."""

from __future__ import annotations

import json
import pathlib
import re
import subprocess
import sys
from collections.abc import Iterator
from typing import Any

ASSIGNMENT = re.compile(
    r"^\s*[\"']?([A-Za-z_][A-Za-z0-9_.-]*)[\"']?\s*[:=]\s*(.*?)\s*$"
)
SECRET_KIND = re.compile(r"^\s*[\"']?kind[\"']?\s*:\s*[\"']?secret[\"']?\s*$", re.I)
EMPTY_VALUES = {"", '""', "''", "null", "~", "{}", "[]"}


def normalized_key(key: str) -> str:
    """Normalize case and separators for credential-key comparisons."""
    return re.sub(r"[^a-z0-9]", "", key.lower())


def category_for_key(key: str) -> str | None:
    """Map a sensitive assignment key to its stable diagnostic category."""
    normalized = normalized_key(key)
    if normalized == "masterkey":
        return "SECRET002"
    if normalized == "webhooksecret":
        return "SECRET003"
    if normalized == "authorization" or normalized.endswith("authorization"):
        return "SECRET004"
    if "otlp" in normalized and normalized.endswith("headers"):
        return "SECRET005"
    if (
        "password" in normalized
        or "passwd" in normalized
        or normalized
        in {
            "token",
            "admintoken",
            "accesstoken",
            "refreshtoken",
            "bearertoken",
            "apikey",
            "clientsecret",
            "privatekey",
            "secret",
        }
        or normalized.endswith("apitoken")
        or normalized.endswith("apikey")
        or normalized.endswith("clientsecret")
        or normalized.endswith("privatekey")
    ):
        return "SECRET007"
    return None


def category_for_value(value: Any) -> str | None:
    """Recognize prohibited fixture sentinels without returning their value."""
    if not isinstance(value, str):
        return None
    normalized = value.strip().strip('"\'').rstrip(",")
    categories = {
        "fixture-token": "SECRET001",
        "master-key": "SECRET002",
        "webhook-secret": "SECRET003",
    }
    return categories.get(normalized)


def reference_context(path: tuple[str, ...], key: str) -> bool:
    """Allow non-secret names/keys used only to reference an external Secret."""
    normalized_path = tuple(normalized_key(component) for component in path)
    normalized = normalized_key(key)
    if normalized_path[-1:] == ("secretkeyref",) and normalized in {
        "name",
        "key",
        "optional",
    }:
        return True
    if "existingsecret" in normalized_path and (
        "keys" in normalized_path or normalized == "name"
    ):
        return True
    return False


def scan_document(document: Any, path: tuple[str, ...] = ()) -> Iterator[str]:
    """Yield categories found in one parsed YAML or JSON document."""
    if isinstance(document, dict):
        normalized_mapping = {
            normalized_key(str(key)): value for key, value in document.items()
        }
        if str(normalized_mapping.get("kind", "")).lower() == "secret":
            yield "SECRET006"
            return

        for key, value in document.items():
            string_key = str(key)
            if not reference_context(path, string_key) and isinstance(value, str) and value:
                value_category = category_for_value(value)
                if value_category is not None:
                    yield value_category
                    continue
                key_category = category_for_key(string_key)
                if key_category is not None:
                    yield key_category
                    continue
            yield from scan_document(value, (*path, string_key))
    elif isinstance(document, list):
        for value in document:
            yield from scan_document(value, path)


def parse_documents(file_path: pathlib.Path) -> list[Any] | None:
    """Parse JSON directly and YAML through the pinned yq parser when possible."""
    try:
        if file_path.suffix.lower() == ".json":
            return [json.loads(file_path.read_text(encoding="utf-8"))]
        if file_path.suffix.lower() not in {".yaml", ".yml"}:
            return None
        result = subprocess.run(
            ["yq", "eval-all", "-o=json", "-I=0", ".", str(file_path)],
            check=True,
            capture_output=True,
            text=True,
        )
        return [json.loads(line) for line in result.stdout.splitlines() if line]
    except (OSError, UnicodeError, json.JSONDecodeError, subprocess.CalledProcessError):
        return None


def scan_text(contents: str) -> set[tuple[int, str]]:
    """Scan non-structured templates and documentation assignment-by-assignment."""
    findings: set[tuple[int, str]] = set()
    for line_number, line in enumerate(contents.splitlines(), start=1):
        if SECRET_KIND.fullmatch(line):
            findings.add((line_number, "SECRET006"))
            continue
        match = ASSIGNMENT.match(line)
        if match is None:
            continue
        key, raw_value = match.groups()
        if raw_value.lower() in EMPTY_VALUES:
            continue
        category = category_for_value(raw_value) or category_for_key(key)
        if category is not None:
            findings.add((line_number, category))
    return findings


def scan_file(file_path: pathlib.Path) -> set[tuple[int, str]]:
    """Return value-free line/category findings for one file."""
    try:
        contents = file_path.read_text(encoding="utf-8")
    except (OSError, UnicodeError):
        return set()
    if "\x00" in contents:
        return set()

    documents = parse_documents(file_path)
    if documents is None:
        return scan_text(contents)
    return {(1, category) for document in documents for category in scan_document(document)}


def main() -> None:
    """Scan all requested files and emit only path, line, and category."""
    if len(sys.argv) < 2:
        raise SystemExit(f"usage: {pathlib.Path(sys.argv[0]).name} FILE...")
    matched = False
    for argument in sys.argv[1:]:
        file_path = pathlib.Path(argument)
        for line_number, category in sorted(scan_file(file_path)):
            print(f"{file_path}:{line_number}:{category}", file=sys.stderr)
            matched = True
    raise SystemExit(1 if matched else 0)


if __name__ == "__main__":
    main()
