#!/usr/bin/env bash
set -Eeuo pipefail

readonly IMAGE="${1:-}"
readonly DATA_DIRECTORY="/var/lib/github-webhook-exporter"
readonly BINARY="/usr/local/bin/github_webhook_exporter"
AUDIT_IMAGE="docker.io/library/busybox:1.37.0-uclibc@"
AUDIT_IMAGE+="sha256:39a19081c6dc060c4efa25ca5845dc867e3f476e86ba8e7ee9da97bc1c2752ca"
readonly AUDIT_IMAGE

if [[ -z "${IMAGE}" ]]; then
    printf 'usage: %s IMAGE\n' "${0##*/}" >&2
    exit 2
fi

for command in base64 curl date docker grep head mktemp rm sleep tar timeout tr; do
    if ! command -v "${command}" >/dev/null 2>&1; then
        printf 'required command not found: %s\n' "${command}" >&2
        exit 2
    fi
done

RESOURCE_SUFFIX="$$-$(date +%s)"
readonly RESOURCE_SUFFIX
readonly AUDIT_CONTAINER="ghe-image-audit-${RESOURCE_SUFFIX}"
readonly PRIMARY_CONTAINER="ghe-image-primary-${RESOURCE_SUFFIX}"
readonly RESTART_CONTAINER="ghe-image-restart-${RESOURCE_SUFFIX}"
readonly DATA_VOLUME="ghe-image-data-${RESOURCE_SUFFIX}"
TEMPORARY_DIRECTORY="$(mktemp -d)"
readonly TEMPORARY_DIRECTORY

cleanup() {
    docker rm --force \
        "${AUDIT_CONTAINER}" \
        "${PRIMARY_CONTAINER}" \
        "${RESTART_CONTAINER}" >/dev/null 2>&1 || true
    docker volume rm --force "${DATA_VOLUME}" >/dev/null 2>&1 || true
    rm -rf "${TEMPORARY_DIRECTORY}"
}
trap cleanup EXIT

fail() {
    printf 'container smoke check failed: %s\n' "$1" >&2
    exit 1
}

assert_equal() {
    local expected="$1"
    local actual="$2"
    local contract="$3"

    if [[ "${actual}" != "${expected}" ]]; then
        printf 'container smoke check failed: %s (expected %q, got %q)\n' \
            "${contract}" "${expected}" "${actual}" >&2
        exit 1
    fi
}

wait_until_ready() {
    local container="$1"
    local deadline=$((SECONDS + 30))
    local mapping
    local port

    while (( SECONDS < deadline )); do
        mapping="$(docker port "${container}" 8080/tcp 2>/dev/null | head -n 1 || true)"
        port="${mapping##*:}"
        if [[ "${port}" =~ ^[0-9]+$ ]] && \
            curl --fail --silent --show-error \
                "http://127.0.0.1:${port}/health/ready" >/dev/null 2>&1; then
            return 0
        fi
        if [[ "$(docker inspect --format '{{.State.Running}}' "${container}")" != "true" ]]; then
            break
        fi
        sleep 1
    done

    docker logs "${container}" >&2 || true
    fail "${container} did not become ready"
}

stop_gracefully() {
    local container="$1"
    local exit_status

    docker kill --signal TERM "${container}" >/dev/null
    if ! exit_status="$(timeout 6 docker wait "${container}")"; then
        docker logs "${container}" >&2 || true
        fail "${container} exceeded the shutdown deadline"
    fi
    assert_equal "0" "${exit_status}" "SIGTERM must produce a successful process exit"
}

MASTER_KEY="$(head -c 32 /dev/urandom | base64 | tr -d '\n')"
readonly MASTER_KEY
ADMIN_TOKEN="$(head -c 24 /dev/urandom | base64 | tr -d '\n')"
readonly ADMIN_TOKEN

assert_equal "linux" \
    "$(docker image inspect --format '{{.Os}}' "${IMAGE}")" \
    "image operating system"
assert_equal "amd64" \
    "$(docker image inspect --format '{{.Architecture}}' "${IMAGE}")" \
    "image architecture"
assert_equal "65532:65532" \
    "$(docker image inspect --format '{{.Config.User}}' "${IMAGE}")" \
    "non-root runtime identity"
assert_equal "[\"${BINARY}\"]" \
    "$(docker image inspect --format '{{json .Config.Entrypoint}}' "${IMAGE}")" \
    "direct binary entrypoint"
assert_equal "null" \
    "$(docker image inspect --format '{{json .Config.Cmd}}' "${IMAGE}")" \
    "absence of a command wrapper"
assert_equal "{\"8080/tcp\":{}}" \
    "$(docker image inspect --format '{{json .Config.ExposedPorts}}' "${IMAGE}")" \
    "single exposed application port"

IMAGE_METADATA="$(
    docker image inspect "${IMAGE}" \
        && docker history --no-trunc "${IMAGE}"
)"
readonly IMAGE_METADATA
if grep --fixed-strings --quiet "${MASTER_KEY}" <<<"${IMAGE_METADATA}"; then
    fail "generated master key appears in image metadata or history"
fi
if grep --fixed-strings --quiet "${ADMIN_TOKEN}" <<<"${IMAGE_METADATA}"; then
    fail "generated administrator token appears in image metadata or history"
fi

docker create --name "${AUDIT_CONTAINER}" "${IMAGE}" >/dev/null
docker export --output "${TEMPORARY_DIRECTORY}/rootfs.tar" "${AUDIT_CONTAINER}"
tar --list --file "${TEMPORARY_DIRECTORY}/rootfs.tar" \
    >"${TEMPORARY_DIRECTORY}/rootfs-files.txt"
ROOTFS_FILES="$(<"${TEMPORARY_DIRECTORY}/rootfs-files.txt")"
readonly ROOTFS_FILES

grep --fixed-strings --line-regexp --quiet \
    "${BINARY#/}" <<<"${ROOTFS_FILES}" || fail "application binary is absent"
grep --fixed-strings --line-regexp --quiet \
    "${DATA_DIRECTORY#/}/" <<<"${ROOTFS_FILES}" || fail "application data directory is absent"

FORBIDDEN_PATH_PATTERN='(^|/)(sh|bash|dash|ash|rustc|cargo|Cargo\.toml|Cargo\.lock)$|'
FORBIDDEN_PATH_PATTERN+='(^|/)\.cargo/registry(/|$)|(^|/)src/.*\.rs$|(^|/)build/src(/|$)'
readonly FORBIDDEN_PATH_PATTERN
if grep --extended-regexp --quiet "${FORBIDDEN_PATH_PATTERN}" <<<"${ROOTFS_FILES}"; then
    grep --extended-regexp "${FORBIDDEN_PATH_PATTERN}" <<<"${ROOTFS_FILES}" >&2
    fail "development or shell artifacts appear in the runtime filesystem"
fi

docker volume create "${DATA_VOLUME}" >/dev/null
docker run --detach \
    --name "${PRIMARY_CONTAINER}" \
    --publish 127.0.0.1::8080 \
    --volume "${DATA_VOLUME}:${DATA_DIRECTORY}" \
    --env "GHE_DATABASE_PATH=${DATA_DIRECTORY}/github-webhook-exporter.db" \
    --env "GHE_MASTER_KEY=${MASTER_KEY}" \
    --env "GHE_ADMIN_TOKEN=${ADMIN_TOKEN}" \
    --env GHE_SHUTDOWN_TIMEOUT_SECONDS=3 \
    --env GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS=2 \
    "${IMAGE}" >/dev/null
wait_until_ready "${PRIMARY_CONTAINER}"

RUNTIME_OWNERS="$(
    docker run --rm \
        --platform linux/amd64 \
        --user 0:0 \
        --volume "${DATA_VOLUME}:${DATA_DIRECTORY}:ro" \
        --entrypoint /bin/sh \
        "${AUDIT_IMAGE}" \
        -c '
            set -e
            /bin/stat -c "%u:%g" "$1" "$2"
            for sidecar in "$2-wal" "$2-shm"; do
                [ ! -e "$sidecar" ] || /bin/stat -c "%u:%g" "$sidecar"
            done
        ' ownership-audit \
        "${DATA_DIRECTORY}" \
        "${DATA_DIRECTORY}/github-webhook-exporter.db"
)"
readonly RUNTIME_OWNERS
declare -a runtime_owner_values
mapfile -t runtime_owner_values <<<"${RUNTIME_OWNERS}"
if (( ${#runtime_owner_values[@]} < 2 )); then
    fail "SQLite runtime ownership audit omitted a required path"
fi
for runtime_owner in "${runtime_owner_values[@]}"; do
    assert_equal "65532:65532" "${runtime_owner}" "SQLite runtime ownership"
done
stop_gracefully "${PRIMARY_CONTAINER}"

docker run --detach \
    --name "${RESTART_CONTAINER}" \
    --publish 127.0.0.1::8080 \
    --volume "${DATA_VOLUME}:${DATA_DIRECTORY}" \
    --env "GHE_DATABASE_PATH=${DATA_DIRECTORY}/github-webhook-exporter.db" \
    --env "GHE_MASTER_KEY=${MASTER_KEY}" \
    --env "GHE_ADMIN_TOKEN=${ADMIN_TOKEN}" \
    --env GHE_SHUTDOWN_TIMEOUT_SECONDS=3 \
    --env GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS=2 \
    "${IMAGE}" >/dev/null
wait_until_ready "${RESTART_CONTAINER}"
stop_gracefully "${RESTART_CONTAINER}"

printf 'container smoke checks passed for %s\n' "${IMAGE}"
