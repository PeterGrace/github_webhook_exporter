#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY
readonly RELEASE_ANNOUNCER="${SCRIPT_DIRECTORY}/release-announce.sh"
readonly VERSION="1.2.3"
readonly RELEASE_TAG="v${VERSION}"
TEMPORARY_DIRECTORY=""
FAKE_BIN_DIRECTORY=""
COMMAND_LOG=""
NOTES_COPY=""
WORK_DIRECTORY=""
CHART_ARCHIVE=""
RUN_STATUS=0
RUN_STDOUT=""
RUN_STDERR=""

fail() {
    printf 'release announcement test failed: %s\n' "$1" >&2
    exit 1
}

cleanup() {
    if [[ -n "${TEMPORARY_DIRECTORY}" ]]; then
        rm -rf -- "${TEMPORARY_DIRECTORY}"
    fi
}

trap cleanup EXIT

# The fake gh records every invocation and copies the generated notes so the test can assert on
# both the command contract and the rendered body without reaching GitHub.
create_fake_gh() {
    FAKE_BIN_DIRECTORY="${TEMPORARY_DIRECTORY}/bin"
    mkdir -p -- "${FAKE_BIN_DIRECTORY}"
    COMMAND_LOG="${TEMPORARY_DIRECTORY}/command.log"
    NOTES_COPY="${TEMPORARY_DIRECTORY}/created-notes.md"
    export COMMAND_LOG NOTES_COPY

    cat >"${FAKE_BIN_DIRECTORY}/gh" <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail

{
    printf 'gh'
    if (($# > 0)); then
        printf ' %s' "$@"
    fi
    printf '\n'
} >>"${COMMAND_LOG}"

# Release lookups go through the REST endpoint, so the fixtures reproduce GitHub's response
# envelopes rather than gh's human-readable diagnostics.
if (($# >= 2)) && [[ "$1" == "api" && "$2" == repos/* ]]; then
    case "${FAKE_RELEASE_STATE:-missing}" in
        present)
            printf '{"tag_name":"%s"}\n' "${2##*/}"
            exit 0
            ;;
        missing)
            printf '{"message":"Not Found","status":"404"}'
            printf 'gh: Not Found (HTTP 404)\n' >&2
            exit 1
            ;;
        error)
            printf '{"message":"Server Error","status":"500"}'
            printf 'gh: Server Error (HTTP 500)\n' >&2
            exit 1
            ;;
        unreadable)
            printf 'not json at all'
            printf 'gh: connection reset\n' >&2
            exit 1
            ;;
        *)
            printf 'unsupported fake release state\n' >&2
            exit 64
            ;;
    esac
fi

if (($# >= 2)) && [[ "$1" == "release" && "$2" == "create" ]]; then
    while (($# > 0)); do
        if [[ "$1" == "--notes-file" ]]; then
            cp -- "$2" "${NOTES_COPY}"
            break
        fi
        shift
    done
    if [[ "${FAKE_RELEASE_CREATE_STATE:-ok}" == "error" ]]; then
        printf 'could not create release\n' >&2
        exit 1
    fi
    exit 0
fi

printf 'unexpected gh invocation\n' >&2
exit 64
EOF
    chmod +x "${FAKE_BIN_DIRECTORY}/gh"
}

create_repository() {
    WORK_DIRECTORY="${TEMPORARY_DIRECTORY}/repository"
    mkdir -p -- "${WORK_DIRECTORY}/dist/release"
    (
        cd "${WORK_DIRECTORY}"
        git init --quiet --initial-branch=main .
        git config user.name 'Announce Test'
        git config user.email 'announce-test@example.invalid'
        printf 'initial\n' >source.txt
        git add source.txt
        git commit --quiet --message 'feat: add the exporter'
        git tag --annotate "${RELEASE_TAG}" --message "Release ${RELEASE_TAG}"
    )
    CHART_ARCHIVE="${WORK_DIRECTORY}/dist/release/github-webhook-exporter-${VERSION}.tgz"
    printf 'chart archive\n' >"${CHART_ARCHIVE}"
}

reset_fixture_state() {
    : >"${COMMAND_LOG}"
    rm -f -- "${NOTES_COPY}"
    unset FAKE_RELEASE_STATE FAKE_RELEASE_CREATE_STATE
}

run_announcer() {
    local stdout_path="${TEMPORARY_DIRECTORY}/stdout"
    local stderr_path="${TEMPORARY_DIRECTORY}/stderr"

    set +e
    (
        cd "${WORK_DIRECTORY}"
        PATH="${FAKE_BIN_DIRECTORY}:${PATH}" "${RELEASE_ANNOUNCER}" "$@"
    ) >"${stdout_path}" 2>"${stderr_path}"
    RUN_STATUS=$?
    set -e
    RUN_STDOUT="$(<"${stdout_path}")"
    RUN_STDERR="$(<"${stderr_path}")"
}

assert_success() {
    (( RUN_STATUS == 0 )) || fail "expected success, got status ${RUN_STATUS}: ${RUN_STDERR}"
    [[ "${RUN_STDOUT}" == *"$1"* ]] || fail "expected stdout to contain '$1', got: ${RUN_STDOUT}"
}

assert_failure() {
    local expected_status="$1"
    local expected_fragment="$2"
    (( RUN_STATUS == expected_status )) \
        || fail "expected status ${expected_status}, got ${RUN_STATUS}"
    [[ "${RUN_STDERR}" == *"${expected_fragment}"* ]] \
        || fail "expected stderr to contain '${expected_fragment}', got: ${RUN_STDERR}"
}

# The announcer writes its notes into a private temporary directory, so the recorded path is
# normalized before comparison while every other argument is matched exactly.
assert_command_log() {
    local expected actual
    expected="$(printf '%s\n' "$@")"
    actual="$(sed 's|--notes-file [^ ]*|--notes-file <notes>|' "${COMMAND_LOG}")"
    [[ "${actual}" == "${expected}" ]] \
        || fail "unexpected gh invocations:"$'\n'"${actual}"$'\n'"expected:"$'\n'"${expected}"
}

assert_no_create_logged() {
    ! grep -q 'gh release create' "${COMMAND_LOG}" \
        || fail 'release creation ran when it must not have'
}

assert_publishes_missing_release() {
    reset_fixture_state
    export FAKE_RELEASE_STATE="missing"
    run_announcer "${VERSION}" "${CHART_ARCHIVE}"
    assert_success "published release notes for ${RELEASE_TAG}"
    assert_command_log \
        "gh api repos/{owner}/{repo}/releases/tags/${RELEASE_TAG}" \
        "gh release create ${RELEASE_TAG} --title ${RELEASE_TAG} --notes-file <notes> --verify-tag ${CHART_ARCHIVE}"
    [[ -f "${NOTES_COPY}" ]] || fail 'release creation received no notes file'
    grep -q '^## Features$' "${NOTES_COPY}" \
        || fail 'release notes omit the grouped landed commits'
    grep -q 'add the exporter' "${NOTES_COPY}" \
        || fail 'release notes omit a landed commit subject'
}

assert_skips_existing_release() {
    reset_fixture_state
    export FAKE_RELEASE_STATE="present"
    run_announcer "${VERSION}" "${CHART_ARCHIVE}"
    assert_success "release notes already published for ${RELEASE_TAG}"
    assert_command_log "gh api repos/{owner}/{repo}/releases/tags/${RELEASE_TAG}"
    assert_no_create_logged
}

assert_fails_closed_on_inspection_error() {
    reset_fixture_state
    export FAKE_RELEASE_STATE="error"
    run_announcer "${VERSION}" "${CHART_ARCHIVE}"
    assert_failure 1 'release inspection failed'
    assert_no_create_logged
}

assert_fails_closed_on_unreadable_inspection() {
    reset_fixture_state
    export FAKE_RELEASE_STATE="unreadable"
    run_announcer "${VERSION}" "${CHART_ARCHIVE}"
    assert_failure 1 'release inspection failed'
    assert_no_create_logged
}

assert_fails_closed_on_creation_error() {
    reset_fixture_state
    export FAKE_RELEASE_STATE="missing" FAKE_RELEASE_CREATE_STATE="error"
    run_announcer "${VERSION}" "${CHART_ARCHIVE}"
    assert_failure 1 'release creation failed'
}

assert_rejects_invalid_requests() {
    reset_fixture_state
    export FAKE_RELEASE_STATE="missing"

    run_announcer "${RELEASE_TAG}" "${CHART_ARCHIVE}"
    assert_failure 1 'version must be a canonical semantic version'

    run_announcer "${VERSION}" "${WORK_DIRECTORY}/dist/release/absent.tgz"
    assert_failure 1 'chart archive is missing'

    local misnamed="${WORK_DIRECTORY}/dist/release/github-webhook-exporter-9.9.9.tgz"
    printf 'chart archive\n' >"${misnamed}"
    run_announcer "${VERSION}" "${misnamed}"
    assert_failure 1 "chart archive must be named github-webhook-exporter-${VERSION}.tgz"
    rm -f -- "${misnamed}"

    run_announcer "${VERSION}"
    assert_failure 2 'usage:'

    assert_no_create_logged
}

main() {
    command -v git >/dev/null 2>&1 || fail 'required command not found: git'

    TEMPORARY_DIRECTORY="$(mktemp -d)"
    create_fake_gh
    create_repository

    assert_publishes_missing_release
    assert_skips_existing_release
    assert_fails_closed_on_inspection_error
    assert_fails_closed_on_unreadable_inspection
    assert_fails_closed_on_creation_error
    assert_rejects_invalid_requests

    printf 'release announcement tests passed\n'
}

main "$@"
