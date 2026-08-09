#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

SCRIPT_DIRECTORY="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY
# shellcheck source=scripts/helm-kind-lifecycle-lib.sh
source "${SCRIPT_DIRECTORY}/helm-kind-lifecycle-lib.sh"

TEMPORARY_DIRECTORY="$(mktemp -d)"
readonly TEMPORARY_DIRECTORY
cleanup() {
    rm -rf "${TEMPORARY_DIRECTORY}"
}
trap cleanup EXIT

readonly SECRET_FILE="${TEMPORARY_DIRECTORY}/secret"
readonly PAYLOAD_FILE="${TEMPORARY_DIRECTORY}/payload.json"
readonly STATUS_FILE="${TEMPORARY_DIRECTORY}/statuses.txt"
readonly ARTIFACT_DIRECTORY="${TEMPORARY_DIRECTORY}/artifacts"
readonly SENTINEL='private-artifact-sentinel'
printf '%s' 'test-signing-secret' >"${SECRET_FILE}"
printf '%s' '{"action":"opened"}' >"${PAYLOAD_FILE}"
mkdir "${ARTIFACT_DIRECTORY}"
printf '%s\n' 'normalized diagnostic' >"${ARTIFACT_DIRECTORY}/clean.txt"

readonly EXPECTED_SIGNATURE='sha256=c50bc5b90b22c2e78d3e3d73c2398ab467f23489f3144ac18b662c010e8c409c'
actual_signature="$(hmac_sha256 "${SECRET_FILE}" "${PAYLOAD_FILE}")"
assert_equal "${EXPECTED_SIGNATURE}" "${actual_signature}" 'HMAC-SHA256 signature'

record_http_status "${STATUS_FILE}" repository_create 201
assert_equal 'repository_create=201' "$(<"${STATUS_FILE}")" 'normalized HTTP status record'

scan_private_artifacts "${ARTIFACT_DIRECTORY}" "${SECRET_FILE}"
printf '%s\n' "${SENTINEL}" >"${SECRET_FILE}"
printf '%s\n' "${SENTINEL}" >"${ARTIFACT_DIRECTORY}/leaked.txt"
scan_error_file="${TEMPORARY_DIRECTORY}/scan-error"
if scan_private_artifacts "${ARTIFACT_DIRECTORY}" "${SECRET_FILE}" \
    2>"${scan_error_file}"; then
    printf 'private artifact scanner accepted a generated credential\n' >&2
    exit 1
fi
if grep --fixed-strings --quiet "${SENTINEL}" "${scan_error_file}"; then
    printf 'private artifact scanner printed matched credential material\n' >&2
    exit 1
fi
grep --fixed-strings --quiet 'leaked.txt' "${scan_error_file}" || {
    printf 'private artifact scanner did not identify the unsafe artifact path\n' >&2
    exit 1
}

readonly FORBIDDEN_PAYLOAD_FILE="${TEMPORARY_DIRECTORY}/forbidden-payload"
printf '%s\n' 'forbidden-payload-fragment' >"${FORBIDDEN_PAYLOAD_FILE}"
rm -f "${ARTIFACT_DIRECTORY}/leaked.txt"
mkdir "${ARTIFACT_DIRECTORY}/nested"
printf '\000forbidden-payload-fragment\000' >"${ARTIFACT_DIRECTORY}/nested/binary.dat"
: >"${scan_error_file}"
if scan_private_artifacts "${ARTIFACT_DIRECTORY}" \
    "${SECRET_FILE}" "${FORBIDDEN_PAYLOAD_FILE}" 2>"${scan_error_file}"; then
    printf 'private artifact scanner ignored a binary payload fragment\n' >&2
    exit 1
fi
if grep --fixed-strings --quiet 'forbidden-payload-fragment' "${scan_error_file}"; then
    printf 'private artifact scanner printed forbidden payload material\n' >&2
    exit 1
fi
grep --fixed-strings --quiet 'nested/binary.dat' "${scan_error_file}" || {
    printf 'private artifact scanner did not identify a nested binary artifact\n' >&2
    exit 1
}

printf 'Kind lifecycle helper contracts passed\n'
