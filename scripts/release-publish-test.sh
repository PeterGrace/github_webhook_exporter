#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY
readonly RELEASE_PUBLISHER="${SCRIPT_DIRECTORY}/release-publish.sh"
readonly VERSION="1.2.3"
readonly IMAGE_REFERENCE="ghcr.io/petergrace/github-webhook-exporter:${VERSION}"
readonly CHART_REFERENCE="oci://ghcr.io/petergrace/charts/github-webhook-exporter"
readonly CHART_REFERENCE_PATH="${CHART_REFERENCE#oci://}"
readonly CHART_REPOSITORY="oci://ghcr.io/petergrace/charts"
readonly FAKE_LOCAL_IMAGE_ID_DEFAULT="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
REAL_MKTEMP="$(command -v mktemp)"
readonly REAL_MKTEMP
TEMPORARY_DIRECTORY=""
FAKE_BIN_DIRECTORY=""
COMMAND_LOG=""
MKTEMP_LOG=""
PUBLISHER_TEMP_ROOT=""
DUMMY_ARCHIVE=""
RUN_STATUS=0
RUN_STDOUT=""
RUN_STDERR=""

fail() {
    printf 'release publication test failed: %s\n' "$1" >&2
    exit 1
}

cleanup() {
    if [[ -n "${TEMPORARY_DIRECTORY}" ]]; then
        rm -rf -- "${TEMPORARY_DIRECTORY}"
    fi
}

create_fake_commands() {
    FAKE_BIN_DIRECTORY="${TEMPORARY_DIRECTORY}/bin"
    mkdir -p -- "${FAKE_BIN_DIRECTORY}"
    COMMAND_LOG="${TEMPORARY_DIRECTORY}/command.log"
    export COMMAND_LOG CHART_REFERENCE_PATH
    : >"${COMMAND_LOG}"

    cat >"${FAKE_BIN_DIRECTORY}/docker" <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail

log_command() {
    printf '%s' "docker" >>"${COMMAND_LOG}"
    if (($# > 0)); then
        printf ' %s' "$@" >>"${COMMAND_LOG}"
    fi
    printf '\n' >>"${COMMAND_LOG}"
}

log_command "$@"

if (($# >= 4)) && [[ "$1" == "image" && "$2" == "inspect" && "$3" == "--format" ]]; then
    if [[ "${FAKE_LOCAL_IMAGE_INSPECT_STATE:-ok}" == "error" ]]; then
        printf 'local image inspection failed\n' >&2
        exit 1
    fi
    if [[ "$4" != '{{.Id}}' ]]; then
        printf 'unexpected docker inspect format\n' >&2
        exit 64
    fi
    printf '%s\n' "${FAKE_LOCAL_IMAGE_ID}"
    exit 0
fi

if (($# == 3)) && [[ "$1" == "manifest" && "$2" == "inspect" ]]; then
    case "${FAKE_REMOTE_IMAGE_STATE}" in
        matching)
            printf '{"schemaVersion":2,"config":{"digest":"%s"}}\n' "${FAKE_LOCAL_IMAGE_ID}"
            ;;
        different)
            printf '%s\n' '{"schemaVersion":2,"config":{"digest":"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}'
            ;;
        index)
            printf '%s\n' '{"schemaVersion":2,"manifests":[{"digest":"sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"}]}'
            ;;
        malformed_json)
            printf '%s\n' '{"schemaVersion":2'
            ;;
        missing_config_digest)
            printf '%s\n' '{"schemaVersion":2,"config":{}}'
            ;;
        invalid_config_digest)
            printf '%s\n' '{"schemaVersion":2,"config":{"digest":"sha256:not-a-digest"}}'
            ;;
        missing)
            printf '%s\n' "${FAKE_REMOTE_IMAGE_MISSING_DIAGNOSTIC:-manifest unknown}" >&2
            exit 1
            ;;
        error)
            printf 'authorization failed\n' >&2
            exit 1
            ;;
        *)
            printf 'unexpected remote image state\n' >&2
            exit 64
            ;;
    esac
    exit 0
fi

if (($# == 2)) && [[ "$1" == "push" ]]; then
    if [[ "${FAKE_DOCKER_PUSH_STATE:-ok}" == "error" ]]; then
        printf 'docker push failed\n' >&2
        exit 1
    fi
    printf 'pushed %s\n' "$2"
    exit 0
fi

printf 'unexpected docker invocation\n' >&2
exit 64
EOF
    chmod +x "${FAKE_BIN_DIRECTORY}/docker"

    cat >"${FAKE_BIN_DIRECTORY}/helm" <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail

log_command() {
    printf '%s' "helm" >>"${COMMAND_LOG}"
    if (($# > 0)); then
        printf ' %s' "$@" >>"${COMMAND_LOG}"
    fi
    printf '\n' >>"${COMMAND_LOG}"
}

log_command "$@"

if (($# == 5)) && [[ "$1" == "show" && "$2" == "chart" && "$4" == "--version" ]]; then
    case "${FAKE_CHART_STATE}" in
        present)
            printf 'apiVersion: v2\nname: github-webhook-exporter\nversion: %s\n' "$5"
            ;;
        missing)
            printf '%s: not found\n' "${CHART_REFERENCE_PATH}:$5" >&2
            exit 1
            ;;
        ambiguous)
            printf 'configuration file not found\n' >&2
            exit 1
            ;;
        error)
            printf 'authorization failed\n' >&2
            exit 1
            ;;
        *)
            printf 'unexpected chart state\n' >&2
            exit 64
            ;;
    esac
    exit 0
fi

if (($# == 3)) && [[ "$1" == "push" ]]; then
    if [[ "${FAKE_HELM_PUSH_STATE:-ok}" == "error" ]]; then
        printf 'helm push failed\n' >&2
        exit 1
    fi
    printf 'pushed %s\n' "$2"
    exit 0
fi

printf 'unexpected helm invocation\n' >&2
exit 64
EOF
    chmod +x "${FAKE_BIN_DIRECTORY}/helm"

    cat >"${FAKE_BIN_DIRECTORY}/mktemp" <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail

if (($# == 0)); then
    printf '<none>\n' >>"${MKTEMP_LOG}"
else
    printf '%s\n' "$*" >>"${MKTEMP_LOG}"
fi
exec "${REAL_MKTEMP}" "$@"
EOF
    chmod +x "${FAKE_BIN_DIRECTORY}/mktemp"

    export PATH="${FAKE_BIN_DIRECTORY}:${PATH}"
}

setup_fixtures() {
    TEMPORARY_DIRECTORY="$(mktemp -d)"
    trap cleanup EXIT
    PUBLISHER_TEMP_ROOT="${TEMPORARY_DIRECTORY}/publisher-tmp"
    mkdir -p -- "${PUBLISHER_TEMP_ROOT}"
    COMMAND_LOG="${TEMPORARY_DIRECTORY}/command.log"
    MKTEMP_LOG="${TEMPORARY_DIRECTORY}/mktemp.log"
    export COMMAND_LOG MKTEMP_LOG PUBLISHER_TEMP_ROOT REAL_MKTEMP
    export TMPDIR="${PUBLISHER_TEMP_ROOT}"
    create_fake_commands
    DUMMY_ARCHIVE="${TEMPORARY_DIRECTORY}/github-webhook-exporter-${VERSION}.tgz"
    : >"${DUMMY_ARCHIVE}"
}

reset_fixture_state() {
    export FAKE_LOCAL_IMAGE_ID="${FAKE_LOCAL_IMAGE_ID_DEFAULT}"
    export FAKE_REMOTE_IMAGE_STATE="missing"
    export FAKE_CHART_STATE="missing"
    unset FAKE_REMOTE_IMAGE_MISSING_DIAGNOSTIC
    unset FAKE_LOCAL_IMAGE_INSPECT_STATE
    unset FAKE_DOCKER_PUSH_STATE
    unset FAKE_HELM_PUSH_STATE
    : >"${COMMAND_LOG}"
    : >"${MKTEMP_LOG}"
    RUN_STATUS=0
    RUN_STDOUT=""
    RUN_STDERR=""
}

run_publisher() {
    local stdout_path stderr_path
    stdout_path="${TEMPORARY_DIRECTORY}/stdout"
    stderr_path="${TEMPORARY_DIRECTORY}/stderr"

    set +e
    "${RELEASE_PUBLISHER}" "$@" >"${stdout_path}" 2>"${stderr_path}"
    RUN_STATUS=$?
    set -e

    RUN_STDOUT="$(<"${stdout_path}")"
    RUN_STDERR="$(<"${stderr_path}")"

    local mktemp_invocations
    mktemp_invocations="$(<"${MKTEMP_LOG}")"
    if [[ -n "${mktemp_invocations}" && "${mktemp_invocations}" != "-d" ]]; then
        fail "publisher must create exactly one private temporary directory"
    fi
    if find "${PUBLISHER_TEMP_ROOT}" -mindepth 1 -print -quit | grep -q .; then
        fail "publisher left temporary registry output behind"
    fi
}

assert_signal_cleanup_contract() {
    local publisher_source
    publisher_source="$(<"${RELEASE_PUBLISHER}")"

    for signal_name in EXIT HUP INT TERM; do
        if ! grep -Eq "^trap .* ${signal_name}$" <<<"${publisher_source}"; then
            fail "publisher must install a cleanup trap for ${signal_name}"
        fi
    done
}

assert_status() {
    local expected_status="$1"
    [[ ${RUN_STATUS} -eq ${expected_status} ]] \
        || fail "expected exit status ${expected_status}, received ${RUN_STATUS}: ${RUN_STDERR}"
}

assert_stdout_contains() {
    local expected_fragment="$1"
    [[ "${RUN_STDOUT}" == *"${expected_fragment}"* ]] \
        || fail "missing stdout diagnostic '${expected_fragment}': ${RUN_STDOUT}"
}

assert_stderr_contains() {
    local expected_fragment="$1"
    [[ "${RUN_STDERR}" == *"${expected_fragment}"* ]] \
        || fail "missing stderr diagnostic '${expected_fragment}': ${RUN_STDERR}"
}

assert_command_log() {
    local expected_log
    expected_log="$(printf '%s\n' "$@")"
    if [[ $# -eq 0 ]]; then
        expected_log=""
    fi
    if [[ "$(<"${COMMAND_LOG}")" != "${expected_log}" ]]; then
        fail "unexpected command log: $(printf '%q' "$(<"${COMMAND_LOG}")")"
    fi
}

assert_no_push_logged() {
    if grep -Eq '^(docker|helm) push ' "${COMMAND_LOG}"; then
        fail "failure path recorded an unintended push"
    fi
}

assert_pushes_follow_inspection() {
    local line inspect_complete=0
    while IFS= read -r line; do
        case "${line}" in
            "helm show chart ${CHART_REFERENCE} --version ${VERSION}")
                inspect_complete=1
                ;;
            "docker push "*|"helm push "*)
                [[ ${inspect_complete} -eq 1 ]] || fail "push occurred before inspection completed"
                ;;
        esac
    done <"${COMMAND_LOG}"
}

run_success_case() {
    local image_state="$1"
    local chart_state="$2"
    shift 2

    reset_fixture_state
    export FAKE_REMOTE_IMAGE_STATE="${image_state}"
    export FAKE_CHART_STATE="${chart_state}"

    run_publisher "${VERSION}" "${IMAGE_REFERENCE}" "${DUMMY_ARCHIVE}"
    assert_status 0
    assert_pushes_follow_inspection
    assert_command_log \
        "docker image inspect --format {{.Id}} ${IMAGE_REFERENCE}" \
        "docker manifest inspect ${IMAGE_REFERENCE}" \
        "helm show chart ${CHART_REFERENCE} --version ${VERSION}" \
        "$@"
}

run_failure_case() {
    local expected_stderr_fragment="$1"
    shift

    run_publisher "$@"
    [[ ${RUN_STATUS} -ne 0 ]] || fail "expected failure for arguments: $*"
    assert_stderr_contains "release publication failed: ${expected_stderr_fragment}"
    assert_pushes_follow_inspection
}

run_manifest_parse_failure_case() {
    local image_state="$1"

    reset_fixture_state
    export FAKE_REMOTE_IMAGE_STATE="${image_state}"
    run_failure_case "image inspection failed" \
        "${VERSION}" "${IMAGE_REFERENCE}" "${DUMMY_ARCHIVE}"
    assert_no_push_logged
    assert_command_log \
        "docker image inspect --format {{.Id}} ${IMAGE_REFERENCE}" \
        "docker manifest inspect ${IMAGE_REFERENCE}"
}

main() {
    [[ -x "${RELEASE_PUBLISHER}" ]] || fail "release publication helper is missing"

    setup_fixtures
    assert_signal_cleanup_contract

    run_success_case "missing" "missing" \
        "docker push ${IMAGE_REFERENCE}" \
        "helm push ${DUMMY_ARCHIVE} ${CHART_REPOSITORY}"

    reset_fixture_state
    export FAKE_REMOTE_IMAGE_MISSING_DIAGNOSTIC="no such manifest"
    run_publisher "${VERSION}" "${IMAGE_REFERENCE}" "${DUMMY_ARCHIVE}"
    assert_status 0
    assert_pushes_follow_inspection
    assert_command_log \
        "docker image inspect --format {{.Id}} ${IMAGE_REFERENCE}" \
        "docker manifest inspect ${IMAGE_REFERENCE}" \
        "helm show chart ${CHART_REFERENCE} --version ${VERSION}" \
        "docker push ${IMAGE_REFERENCE}" \
        "helm push ${DUMMY_ARCHIVE} ${CHART_REPOSITORY}"

    run_success_case "matching" "missing" \
        "helm push ${DUMMY_ARCHIVE} ${CHART_REPOSITORY}"
    assert_stdout_contains "chart-only recovery"

    reset_fixture_state
    export FAKE_REMOTE_IMAGE_STATE="different"
    export FAKE_CHART_STATE="missing"
    run_failure_case "digest conflict" "${VERSION}" "${IMAGE_REFERENCE}" "${DUMMY_ARCHIVE}"
    assert_no_push_logged
    assert_command_log \
        "docker image inspect --format {{.Id}} ${IMAGE_REFERENCE}" \
        "docker manifest inspect ${IMAGE_REFERENCE}" \
        "helm show chart ${CHART_REFERENCE} --version ${VERSION}"

    reset_fixture_state
    export FAKE_REMOTE_IMAGE_STATE="index"
    export FAKE_CHART_STATE="missing"
    run_failure_case "unsupported remote image manifest" \
        "${VERSION}" "${IMAGE_REFERENCE}" "${DUMMY_ARCHIVE}"
    assert_no_push_logged
    assert_command_log \
        "docker image inspect --format {{.Id}} ${IMAGE_REFERENCE}" \
        "docker manifest inspect ${IMAGE_REFERENCE}"

    run_manifest_parse_failure_case "malformed_json"
    run_manifest_parse_failure_case "missing_config_digest"
    run_manifest_parse_failure_case "invalid_config_digest"

    reset_fixture_state
    export FAKE_REMOTE_IMAGE_STATE="missing"
    export FAKE_CHART_STATE="present"
    run_failure_case "chart already exists while the image is absent" \
        "${VERSION}" "${IMAGE_REFERENCE}" "${DUMMY_ARCHIVE}"
    assert_no_push_logged
    assert_command_log \
        "docker image inspect --format {{.Id}} ${IMAGE_REFERENCE}" \
        "docker manifest inspect ${IMAGE_REFERENCE}" \
        "helm show chart ${CHART_REFERENCE} --version ${VERSION}"

    reset_fixture_state
    export FAKE_REMOTE_IMAGE_STATE="matching"
    export FAKE_CHART_STATE="present"
    run_failure_case "release is already published" \
        "${VERSION}" "${IMAGE_REFERENCE}" "${DUMMY_ARCHIVE}"
    assert_no_push_logged
    assert_command_log \
        "docker image inspect --format {{.Id}} ${IMAGE_REFERENCE}" \
        "docker manifest inspect ${IMAGE_REFERENCE}" \
        "helm show chart ${CHART_REFERENCE} --version ${VERSION}"

    reset_fixture_state
    export FAKE_REMOTE_IMAGE_STATE="error"
    export FAKE_CHART_STATE="missing"
    run_failure_case "image inspection failed" "${VERSION}" "${IMAGE_REFERENCE}" "${DUMMY_ARCHIVE}"
    assert_no_push_logged
    assert_command_log \
        "docker image inspect --format {{.Id}} ${IMAGE_REFERENCE}" \
        "docker manifest inspect ${IMAGE_REFERENCE}"

    reset_fixture_state
    export FAKE_REMOTE_IMAGE_STATE="missing"
    export FAKE_CHART_STATE="ambiguous"
    run_failure_case "chart inspection failed" "${VERSION}" "${IMAGE_REFERENCE}" "${DUMMY_ARCHIVE}"
    assert_no_push_logged
    assert_command_log \
        "docker image inspect --format {{.Id}} ${IMAGE_REFERENCE}" \
        "docker manifest inspect ${IMAGE_REFERENCE}" \
        "helm show chart ${CHART_REFERENCE} --version ${VERSION}"

    reset_fixture_state
    export FAKE_REMOTE_IMAGE_STATE="missing"
    export FAKE_CHART_STATE="error"
    run_failure_case "chart inspection failed" "${VERSION}" "${IMAGE_REFERENCE}" "${DUMMY_ARCHIVE}"
    assert_no_push_logged
    assert_command_log \
        "docker image inspect --format {{.Id}} ${IMAGE_REFERENCE}" \
        "docker manifest inspect ${IMAGE_REFERENCE}" \
        "helm show chart ${CHART_REFERENCE} --version ${VERSION}"

    reset_fixture_state
    run_failure_case "expected VERSION LOCAL_IMAGE CHART_ARCHIVE" "${VERSION}" "${IMAGE_REFERENCE}"
    assert_no_push_logged
    assert_command_log

    reset_fixture_state
    run_failure_case "version must be a canonical semantic version" \
        "01.2.3" "ghcr.io/petergrace/github-webhook-exporter:01.2.3" "${DUMMY_ARCHIVE}"
    assert_no_push_logged
    assert_command_log

    reset_fixture_state
    run_failure_case "local image must match ${IMAGE_REFERENCE}" \
        "${VERSION}" "ghcr.io/petergrace/github-webhook-exporter:9.9.9" "${DUMMY_ARCHIVE}"
    assert_no_push_logged
    assert_command_log

    local wrong_archive_path="${TEMPORARY_DIRECTORY}/wrong-name.tgz"
    : >"${wrong_archive_path}"
    reset_fixture_state
    run_failure_case "chart archive must be named github-webhook-exporter-${VERSION}.tgz" \
        "${VERSION}" "${IMAGE_REFERENCE}" "${wrong_archive_path}"
    assert_no_push_logged
    assert_command_log

    reset_fixture_state
    run_failure_case "chart archive is missing" \
        "${VERSION}" "${IMAGE_REFERENCE}" "${TEMPORARY_DIRECTORY}/missing.tgz"
    assert_no_push_logged
    assert_command_log

    reset_fixture_state
    export FAKE_LOCAL_IMAGE_INSPECT_STATE="error"
    run_failure_case "local image inspection failed" \
        "${VERSION}" "${IMAGE_REFERENCE}" "${DUMMY_ARCHIVE}"
    assert_no_push_logged
    assert_command_log "docker image inspect --format {{.Id}} ${IMAGE_REFERENCE}"

    reset_fixture_state
    export FAKE_DOCKER_PUSH_STATE="error"
    run_failure_case "image push failed" "${VERSION}" "${IMAGE_REFERENCE}" "${DUMMY_ARCHIVE}"
    assert_command_log \
        "docker image inspect --format {{.Id}} ${IMAGE_REFERENCE}" \
        "docker manifest inspect ${IMAGE_REFERENCE}" \
        "helm show chart ${CHART_REFERENCE} --version ${VERSION}" \
        "docker push ${IMAGE_REFERENCE}"

    reset_fixture_state
    export FAKE_HELM_PUSH_STATE="error"
    run_failure_case "chart push failed after the image was published" \
        "${VERSION}" "${IMAGE_REFERENCE}" "${DUMMY_ARCHIVE}"
    assert_stderr_contains "rerunning the same release can recover only if the rebuilt image digest matches"
    assert_command_log \
        "docker image inspect --format {{.Id}} ${IMAGE_REFERENCE}" \
        "docker manifest inspect ${IMAGE_REFERENCE}" \
        "helm show chart ${CHART_REFERENCE} --version ${VERSION}" \
        "docker push ${IMAGE_REFERENCE}" \
        "helm push ${DUMMY_ARCHIVE} ${CHART_REPOSITORY}"

    printf 'release publication tests passed\n'
}

main "$@"
