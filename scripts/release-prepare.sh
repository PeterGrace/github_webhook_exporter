#!/usr/bin/env bash
set -Eeuo pipefail

REPOSITORY_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPOSITORY_ROOT
cd "${REPOSITORY_ROOT}"

fail() {
    printf 'release prepare failed: %s\n' "$1" >&2
    exit 1
}

(($# > 0)) || fail "a cargo-release version level is required"
CURRENT_BRANCH="$(git rev-parse --abbrev-ref HEAD)"
readonly CURRENT_BRANCH
[[ "${CURRENT_BRANCH}" == "main" ]] || fail "must prepare from main, not ${CURRENT_BRANCH}"
[[ -z "$(git status --porcelain)" ]] || fail "working tree is not clean"

git fetch origin main
LOCAL_HEAD="$(git rev-parse HEAD)"
REMOTE_HEAD="$(git rev-parse refs/remotes/origin/main)"
readonly LOCAL_HEAD REMOTE_HEAD
[[ "${LOCAL_HEAD}" == "${REMOTE_HEAD}" ]] \
    || fail "local main is not exactly origin/main; update main before preparing the release"

exec cargo release --no-publish --no-verify --no-push "$@" --execute
