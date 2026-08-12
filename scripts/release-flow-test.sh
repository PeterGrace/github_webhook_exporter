#!/usr/bin/env bash
set -Eeuo pipefail

SOURCE_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly SOURCE_ROOT
TEMPORARY_DIRECTORY="$(mktemp -d)"
readonly TEMPORARY_DIRECTORY
trap 'rm -rf -- "${TEMPORARY_DIRECTORY}"' EXIT

fail() {
    printf 'release flow test failed: %s\n' "$1" >&2
    exit 1
}

configure_identity() {
    git config user.name 'Release Test'
    git config user.email 'release-test@example.invalid'
}

create_repository() {
    local name="$1"
    local remote="${TEMPORARY_DIRECTORY}/${name}.git"
    local work="${TEMPORARY_DIRECTORY}/${name}"
    git init --bare --initial-branch=main "${remote}" >/dev/null
    git clone "${remote}" "${work}" >/dev/null 2>&1
    (
        cd "${work}"
        configure_identity
        mkdir -p scripts
        cp "${SOURCE_ROOT}/scripts/release-prepare.sh" scripts/
        cp "${SOURCE_ROOT}/scripts/release-ship.sh" scripts/
        cat >scripts/release-version.sh <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
[[ "$1" == "v1.2.3" ]]
EOF
        chmod +x scripts/*.sh
        printf 'initial\n' >source.txt
        git add .
        git commit -m initial >/dev/null
        git push origin main >/dev/null
    )
    printf '%s\n' "${work}"
}

assert_prepare_rejects_stale_main() {
    local work upstream output status
    work="$(create_repository prepare-stale)"
    upstream="${TEMPORARY_DIRECTORY}/prepare-upstream"
    git clone "${TEMPORARY_DIRECTORY}/prepare-stale.git" "${upstream}" >/dev/null 2>&1
    (
        cd "${upstream}"
        configure_identity
        printf 'new\n' >>source.txt
        git commit -am upstream >/dev/null
        git push origin main >/dev/null
    )

    set +e
    output="$(cd "${work}" && scripts/release-prepare.sh patch 2>&1)"
    status=$?
    set -e
    [[ ${status} -ne 0 ]] || fail 'stale main was accepted for release preparation'
    [[ "${output}" == *'local main is not exactly origin/main'* ]] \
        || fail "missing stale-main diagnostic: ${output}"
}

assert_ship_rejects_release_based_on_stale_main() {
    local work upstream output status
    work="$(create_repository ship-stale)"
    (
        cd "${work}"
        printf 'version 1.2.3\n' >version.txt
        git add version.txt
        git commit -m 'release 1.2.3' >/dev/null
        git tag -a v1.2.3 -m 'Release v1.2.3'
    )
    upstream="${TEMPORARY_DIRECTORY}/ship-stale-upstream"
    git clone "${TEMPORARY_DIRECTORY}/ship-stale.git" "${upstream}" >/dev/null 2>&1
    (
        cd "${upstream}"
        configure_identity
        printf 'merged while preparing\n' >>source.txt
        git commit -am upstream >/dev/null
        git push origin main >/dev/null
    )

    set +e
    output="$(cd "${work}" && scripts/release-ship.sh 2>&1)"
    status=$?
    set -e
    [[ ${status} -ne 0 ]] || fail 'stale release commit was accepted for shipping'
    [[ "${output}" == *'release commit is not based on current origin/main'* ]] \
        || fail "missing stale-release diagnostic: ${output}"
}

create_fake_gh() {
    local bin_directory="$1"
    mkdir -p "${bin_directory}"
    cat >"${bin_directory}/gh" <<'EOF'
#!/usr/bin/env bash
set -Eeuo pipefail
case "$1 $2" in
    'pr create')
        printf 'https://example.invalid/pull/1\n'
        ;;
    'pr merge')
        integration="${TEST_INTEGRATION_CLONE}"
        git -C "${integration}" fetch origin >/dev/null
        git -C "${integration}" checkout main >/dev/null
        git -C "${integration}" reset --hard origin/main >/dev/null
        printf 'landed after release preparation\n' >"${integration}/concurrent.txt"
        git -C "${integration}" add concurrent.txt
        git -C "${integration}" commit -m 'concurrent main change' >/dev/null
        git -C "${integration}" push origin main >/dev/null
        git -C "${integration}" merge --no-ff "origin/${TEST_RELEASE_BRANCH}" \
            -m 'merge release' >/dev/null
        git -C "${integration}" push origin main >/dev/null
        git -C "${integration}" rev-parse HEAD >"${TEST_MERGE_COMMIT_FILE}"
        ;;
    'pr view')
        cat "${TEST_MERGE_COMMIT_FILE}"
        ;;
    *)
        printf 'unexpected gh invocation: %s\n' "$*" >&2
        exit 64
        ;;
esac
EOF
    chmod +x "${bin_directory}/gh"
}

assert_ship_tags_release_merge_commit() {
    local work integration fake_bin merge_file tagged_commit merged_commit
    work="$(create_repository ship-merge-tag)"
    integration="${TEMPORARY_DIRECTORY}/ship-integration"
    git clone "${TEMPORARY_DIRECTORY}/ship-merge-tag.git" "${integration}" >/dev/null 2>&1
    (cd "${integration}" && configure_identity)
    (
        cd "${work}"
        printf 'version 1.2.3\n' >version.txt
        git add version.txt
        git commit -m 'release 1.2.3' >/dev/null
        git tag -a v1.2.3 -m 'Release v1.2.3'
    )
    fake_bin="${TEMPORARY_DIRECTORY}/fake-bin"
    merge_file="${TEMPORARY_DIRECTORY}/merge-commit"
    create_fake_gh "${fake_bin}"

    TEST_INTEGRATION_CLONE="${integration}" \
    TEST_RELEASE_BRANCH='release/v1.2.3' \
    TEST_MERGE_COMMIT_FILE="${merge_file}" \
    PATH="${fake_bin}:${PATH}" \
        bash -c "cd '${work}' && scripts/release-ship.sh" >/dev/null

    tagged_commit="$(git --git-dir="${TEMPORARY_DIRECTORY}/ship-merge-tag.git" rev-parse v1.2.3^{})"
    merged_commit="$(<"${merge_file}")"
    [[ "${tagged_commit}" == "${merged_commit}" ]] \
        || fail "tag points at ${tagged_commit}, expected merge ${merged_commit}"
    git --git-dir="${TEMPORARY_DIRECTORY}/ship-merge-tag.git" \
        cat-file -e "${tagged_commit}:concurrent.txt" \
        || fail 'release tag omitted a change merged concurrently to main'
}

assert_prepare_rejects_stale_main
assert_ship_rejects_release_based_on_stale_main
assert_ship_tags_release_merge_commit
printf 'release flow tests passed\n'
