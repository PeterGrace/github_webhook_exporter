#!/usr/bin/env bash
set -Eeuo pipefail
umask 077

SCRIPT_DIRECTORY="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY
readonly MAINTENANCE_SCRIPT="${SCRIPT_DIRECTORY}/helm-sqlite-maintenance.sh"
TEMPORARY_DIRECTORY="$(mktemp -d)"
readonly TEMPORARY_DIRECTORY
cleanup() {
    rm -rf "${TEMPORARY_DIRECTORY}"
}
trap cleanup EXIT

readonly FAKE_BIN_DIRECTORY="${TEMPORARY_DIRECTORY}/bin"
readonly FAKE_LOG="${TEMPORARY_DIRECTORY}/kubectl.log"
readonly FAKE_MANIFEST="${TEMPORARY_DIRECTORY}/manifest.yaml"
mkdir "${FAKE_BIN_DIRECTORY}"
cat >"${FAKE_BIN_DIRECTORY}/kubectl" <<'FAKE_KUBECTL'
#!/usr/bin/env bash
set -Eeuo pipefail
printf '%q ' "$@" >>"${FAKE_KUBECTL_LOG}"
printf '\n' >>"${FAKE_KUBECTL_LOG}"

arguments=" $* "
if [[ "${arguments}" == *' get statefulset '* ]]; then
    printf '%s' "${FAKE_REPLICAS:-1}"
elif [[ "${arguments}" == *' get pod '* ]]; then
    [[ "${FAKE_POD_EXISTS:-true}" == true ]]
elif [[ "${arguments}" == *' apply -f - '* || "${arguments}" == *' apply -f -' ]]; then
    cat >"${FAKE_KUBECTL_MANIFEST}"
    printf '%s\n' 'pod/sqlite-maintenance created'
elif [[ "${arguments}" == *' logs '* ]]; then
    printf '%s\n' 'maintenance_complete'
fi
FAKE_KUBECTL
chmod 700 "${FAKE_BIN_DIRECTORY}/kubectl"

export PATH="${FAKE_BIN_DIRECTORY}:${PATH}"
export FAKE_KUBECTL_LOG="${FAKE_LOG}"
export FAKE_KUBECTL_MANIFEST="${FAKE_MANIFEST}"
export KUBECTL_CONTEXT='kind-maintenance-test'

fail() {
    printf 'SQLite maintenance test failed: %s\n' "$1" >&2
    exit 1
}

assert_manifest_contains() {
    local expected="$1"
    grep --fixed-strings --quiet -- "${expected}" "${FAKE_MANIFEST}" ||
        fail "rendered Pod omitted required contract: ${expected}"
}

assert_manifest_command() {
    local expected="$1"
    sed 's/^[[:space:]]*//' "${FAKE_MANIFEST}" | \
        grep --fixed-strings --line-regexp --quiet -- "${expected}" ||
        fail "rendered Pod omitted required command: ${expected}"
}

if "${MAINTENANCE_SCRIPT}" backup valid-namespace valid-statefulset valid-pvc \
    '../unsafe.db' >"${TEMPORARY_DIRECTORY}/invalid.out" 2>&1; then
    fail 'accepted an unsafe backup basename'
fi
if [[ -s "${FAKE_LOG}" ]]; then
    fail 'invoked kubectl before rejecting an unsafe backup basename'
fi

: >"${FAKE_LOG}"
FAKE_REPLICAS=1 FAKE_POD_EXISTS=true \
    "${MAINTENANCE_SCRIPT}" backup valid-namespace valid-statefulset valid-pvc \
    backup-20260809.db >"${TEMPORARY_DIRECTORY}/backup.out"
assert_manifest_contains \
    'docker.io/keinos/sqlite3:3.50.4@sha256:d9e50ca08f59d96055c514175f3f4b1fcacaca97fa93508a0334c62eb9de9382'
assert_manifest_contains 'runAsUser: 65532'
assert_manifest_contains 'runAsGroup: 65532'
assert_manifest_contains 'readOnlyRootFilesystem: true'
assert_manifest_contains 'allowPrivilegeEscalation: false'
assert_manifest_contains 'drop: ["ALL"]'
assert_manifest_contains '.backup /data/backup-20260809.db'
assert_manifest_contains 'PRAGMA integrity_check;'
assert_manifest_contains 'chmod 0600 /data/backup-20260809.db'
assert_manifest_contains 'automountServiceAccountToken: false'

: >"${FAKE_LOG}"
export FAKE_REPLICAS=1
export FAKE_POD_EXISTS=true
if "${MAINTENANCE_SCRIPT}" restore valid-namespace valid-statefulset valid-pvc \
    backup-20260809.db >"${TEMPORARY_DIRECTORY}/running.out" 2>&1; then
    fail 'restore succeeded while the StatefulSet requested one replica'
fi
if grep --fixed-strings --quiet 'apply -f -' "${FAKE_LOG}"; then
    fail 'restore created a maintenance Pod while the exporter was active'
fi

: >"${FAKE_LOG}"
export FAKE_REPLICAS=0
export FAKE_POD_EXISTS=false
"${MAINTENANCE_SCRIPT}" restore valid-namespace valid-statefulset valid-pvc \
    backup-20260809.db >"${TEMPORARY_DIRECTORY}/restore.out"
assert_manifest_contains 'PRAGMA integrity_check;'
assert_manifest_contains '.restore /data/backup-20260809.db'
assert_manifest_contains 'chmod 0600 /data/github-webhook-exporter.db.restore'
assert_manifest_contains 'mv /data/github-webhook-exporter.db /data/github-webhook-exporter.db.pre-restore'
assert_manifest_contains 'rm -f /data/github-webhook-exporter.db-wal /data/github-webhook-exporter.db-shm'
assert_manifest_contains 'mv /data/github-webhook-exporter.db.restore /data/github-webhook-exporter.db'
assert_manifest_command 'rm -f /data/github-webhook-exporter.db'
assert_manifest_command \
    'mv /data/github-webhook-exporter.db.pre-restore /data/github-webhook-exporter.db || true'
assert_manifest_contains "stat -c '%u:%g %a' /data/github-webhook-exporter.db"
assert_manifest_contains '65532:65532 600'

printf 'SQLite maintenance contracts passed\n'
