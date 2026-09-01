#!/usr/bin/env bash
set -Eeuo pipefail

SCRIPT_DIRECTORY="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
readonly SCRIPT_DIRECTORY
readonly CHANGELOG_GENERATOR="${SCRIPT_DIRECTORY}/release-changelog.sh"
readonly REPOSITORY="Owner/repository"
TEMPORARY_DIRECTORY=""
RUN_STATUS=0
RUN_STDOUT=""
RUN_STDERR=""

fail() {
    printf 'release changelog test failed: %s\n' "$1" >&2
    exit 1
}

cleanup() {
    if [[ -n "${TEMPORARY_DIRECTORY}" ]]; then
        rm -rf -- "${TEMPORARY_DIRECTORY}"
    fi
}

trap cleanup EXIT

commit_subject() {
    local subject="$1"
    printf '%s\n' "${subject}" >>source.txt
    git add source.txt
    git commit --quiet --message "${subject}"
}

# Build one fixture history covering conventional types, scopes, breaking markers, merge commits,
# non-conventional subjects, and the cargo-release version bump.
create_repository() {
    local work="${TEMPORARY_DIRECTORY}/repository"
    git init --quiet --initial-branch=main "${work}"
    (
        cd "${work}"
        git config user.name 'Changelog Test'
        git config user.email 'changelog-test@example.invalid'

        commit_subject 'feat: add the original exporter'
        git tag --annotate v1.2.2 --message 'Release v1.2.2'

        commit_subject 'fix: stop dropping deliveries'
        commit_subject 'feat(chart): expose the queue depth'
        commit_subject 'feat!: require an explicit collector endpoint'
        commit_subject 'docs: describe the queue depth setting'
        commit_subject 'Land the vendored fixture refresh'
        commit_subject 'wip: leave the parser half-finished'

        # A merge commit reproduces how pull requests land on main; it must never become a bullet.
        git checkout --quiet -b topic
        commit_subject 'test: cover the queue depth boundary'
        git checkout --quiet main
        git merge --quiet --no-ff topic --message 'Merge pull request #7 from Owner/topic'

        commit_subject 'chore: Release github_webhook_exporter version 1.2.3'
        git tag --annotate v1.2.3 --message 'Release v1.2.3'

        # A prerelease tag must never be selected as the comparison base.
        git tag --annotate v1.2.4-rc.1 --message 'Prerelease'
    )
    printf '%s\n' "${work}"
}

run_generator() {
    local work="$1"
    shift
    local stdout_path="${TEMPORARY_DIRECTORY}/stdout"
    local stderr_path="${TEMPORARY_DIRECTORY}/stderr"

    set +e
    (cd "${work}" && "${CHANGELOG_GENERATOR}" "$@") >"${stdout_path}" 2>"${stderr_path}"
    RUN_STATUS=$?
    set -e
    RUN_STDOUT="$(<"${stdout_path}")"
    RUN_STDERR="$(<"${stderr_path}")"
}

assert_success() {
    (( RUN_STATUS == 0 )) || fail "expected success, got status ${RUN_STATUS}: ${RUN_STDERR}"
}

assert_failure() {
    local expected_status="$1"
    local expected_fragment="$2"
    (( RUN_STATUS == expected_status )) \
        || fail "expected status ${expected_status}, got ${RUN_STATUS}"
    [[ "${RUN_STDERR}" == *"${expected_fragment}"* ]] \
        || fail "expected stderr to contain '${expected_fragment}', got: ${RUN_STDERR}"
}

assert_contains() {
    [[ "${RUN_STDOUT}" == *"$1"* ]] || fail "expected notes to contain '$1'"
}

assert_omits() {
    [[ "${RUN_STDOUT}" != *"$1"* ]] || fail "expected notes to omit '$1'"
}

# Confirm the rendered order of two fragments, so sections and bullets stay deterministic.
assert_precedes() {
    local first="$1"
    local second="$2"
    local prefix="${RUN_STDOUT%%"${second}"*}"
    [[ "${prefix}" != "${RUN_STDOUT}" ]] || fail "expected notes to contain '${second}'"
    [[ "${prefix}" == *"${first}"* ]] || fail "expected '${first}' before '${second}'"
}

assert_groups_landed_commits() {
    local work="$1"
    run_generator "${work}" 1.2.3 "${REPOSITORY}"
    assert_success

    assert_contains '## Breaking changes'
    assert_contains 'require an explicit collector endpoint'
    assert_contains '## Features'
    assert_contains '(chart) expose the queue depth'
    assert_contains '## Fixes'
    assert_contains 'stop dropping deliveries'
    assert_contains '## Documentation'
    assert_contains '## Testing'
    assert_contains 'cover the queue depth boundary'
    assert_contains '## Other changes'
    assert_contains 'Land the vendored fixture refresh'
    # An unrecognized type has no section of its own, so it belongs in Other changes with its
    # prefix intact rather than in a bucket that is never rendered.
    assert_contains '- wip: leave the parser half-finished'

    assert_precedes '## Breaking changes' '## Features'
    assert_precedes '## Features' '## Fixes'
    assert_precedes '## Fixes' '## Documentation'
    assert_precedes '## Documentation' '## Testing'
    assert_precedes '## Testing' '## Other changes'
}

# Every landed commit must reach a rendered section. Only a total loss trips the generator's own
# empty guard, so a partial drop would otherwise ship silently.
assert_renders_every_landed_commit() {
    local work="$1"
    local expected actual
    run_generator "${work}" 1.2.3 "${REPOSITORY}"
    assert_success

    expected="$(
        cd "${work}" \
            && git log --no-merges --format='%s' v1.2.2..v1.2.3 \
            | grep -c -v '^chore: Release '
    )"
    actual="$(grep -c '^- ' <<<"${RUN_STDOUT}")"
    (( actual == expected )) \
        || fail "rendered ${actual} bullets for ${expected} landed commits"
}

assert_excludes_release_scaffolding() {
    local work="$1"
    run_generator "${work}" 1.2.3 "${REPOSITORY}"
    assert_success

    assert_omits 'Merge pull request'
    assert_omits 'chore: Release github_webhook_exporter'
    assert_omits '## Chores'
    # The tagged predecessor's own commit belongs to the previous release, not this one.
    assert_omits 'add the original exporter'
}

assert_reports_install_and_comparison() {
    local work="$1"
    run_generator "${work}" 1.2.3 "${REPOSITORY}"
    assert_success

    assert_contains 'docker pull ghcr.io/petergrace/github-webhook-exporter:1.2.3'
    assert_contains \
        'helm install github-webhook-exporter oci://ghcr.io/petergrace/charts/github-webhook-exporter --version 1.2.3'
    assert_contains "https://github.com/${REPOSITORY}/compare/v1.2.2...v1.2.3"
}

assert_first_release_spans_full_history() {
    local work="$1"
    run_generator "${work}" 1.2.2 "${REPOSITORY}"
    assert_success

    assert_contains 'add the original exporter'
    assert_contains "https://github.com/${REPOSITORY}/commits/v1.2.2"
    assert_omits 'compare/'
    assert_omits 'stop dropping deliveries'
}

# Without an explicit argument the generator names the repository CI is running in, so the compare
# link stays correct on a fork.
assert_defaults_repository_to_environment() {
    local work="$1"
    GITHUB_REPOSITORY='Fork/repository' run_generator "${work}" 1.2.3
    assert_success
    assert_contains 'https://github.com/Fork/repository/compare/v1.2.2...v1.2.3'
}

assert_rejects_invalid_requests() {
    local work="$1"

    run_generator "${work}" 'v1.2.3' "${REPOSITORY}"
    assert_failure 1 'version must be a canonical semantic version'

    run_generator "${work}" '1.2' "${REPOSITORY}"
    assert_failure 1 'version must be a canonical semantic version'

    run_generator "${work}" '1.2.3' 'not-a-repository'
    assert_failure 1 'repository must be in OWNER/NAME form'

    run_generator "${work}" '9.9.9' "${REPOSITORY}"
    assert_failure 1 'release tag v9.9.9 is not present in this checkout'

    run_generator "${work}"
    assert_failure 2 'usage:'
}

main() {
    command -v git >/dev/null 2>&1 || fail 'required command not found: git'

    TEMPORARY_DIRECTORY="$(mktemp -d)"
    local work
    work="$(create_repository)"

    assert_groups_landed_commits "${work}"
    assert_renders_every_landed_commit "${work}"
    assert_excludes_release_scaffolding "${work}"
    assert_reports_install_and_comparison "${work}"
    assert_first_release_spans_full_history "${work}"
    assert_defaults_repository_to_environment "${work}"
    assert_rejects_invalid_requests "${work}"

    printf 'release changelog tests passed\n'
}

main "$@"
