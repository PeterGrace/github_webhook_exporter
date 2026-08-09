#!/usr/bin/env bash

# Shared deterministic helpers for the Kind lifecycle acceptance harness.

require_command() {
    local command_name="$1"
    if ! command -v "${command_name}" >/dev/null 2>&1; then
        printf 'Kind lifecycle acceptance failed: required command not found: %s\n' \
            "${command_name}" >&2
        return 1
    fi
}

fail() {
    printf 'Kind lifecycle acceptance failed: %s\n' "$1" >&2
    return 1
}

assert_equal() {
    local expected="$1"
    local actual="$2"
    local contract="$3"
    if [[ "${actual}" != "${expected}" ]]; then
        printf 'Kind lifecycle acceptance failed: %s (expected %q, got %q)\n' \
            "${contract}" "${expected}" "${actual}" >&2
        return 1
    fi
}

hmac_sha256() {
    local secret_file="$1"
    local payload_file="$2"
    if [[ ! -s "${secret_file}" || ! -f "${payload_file}" ]]; then
        fail 'HMAC input file is missing or empty'
        return 1
    fi
    python3 - "${secret_file}" "${payload_file}" <<'PY'
import hashlib
import hmac
import pathlib
import sys

secret = pathlib.Path(sys.argv[1]).read_bytes()
payload = pathlib.Path(sys.argv[2]).read_bytes()
print(f"sha256={hmac.new(secret, payload, hashlib.sha256).hexdigest()}")
PY
}

record_http_status() {
    local output_file="$1"
    local label="$2"
    local status="$3"

    if [[ ! "${label}" =~ ^[a-z0-9_]+$ || ! "${status}" =~ ^[0-9]{3}$ ]]; then
        fail 'invalid normalized HTTP status record'
        return 1
    fi
    printf '%s=%s\n' "${label}" "${status}" >>"${output_file}"
}

scan_private_artifacts() {
    local artifact_directory="$1"
    shift
    local pattern_file
    local artifact_file
    local unsafe=false

    if [[ ! -d "${artifact_directory}" || "$#" -eq 0 ]]; then
        fail 'artifact scan requires a directory and private pattern files'
        return 1
    fi

    for pattern_file in "$@"; do
        if [[ ! -s "${pattern_file}" ]]; then
            fail 'artifact scan pattern file is missing or empty'
            return 1
        fi
        while IFS= read -r -d '' artifact_file; do
            if grep --binary-files=without-match --fixed-strings --quiet \
                --file="${pattern_file}" -- "${artifact_file}"; then
                printf 'Kind lifecycle acceptance failed: private material found in artifact: %s\n' \
                    "${artifact_file#"${artifact_directory}/"}" >&2
                unsafe=true
            fi
        done < <(find "${artifact_directory}" -type f -print0)
    done

    [[ "${unsafe}" == false ]]
}
