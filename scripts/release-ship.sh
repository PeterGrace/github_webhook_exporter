#!/usr/bin/env bash
set -Eeuo pipefail

REPOSITORY_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
readonly REPOSITORY_ROOT
cd "${REPOSITORY_ROOT}"

fail() {
    printf 'release ship failed: %s\n' "$1" >&2
    exit 1
}

CURRENT_BRANCH="$(git rev-parse --abbrev-ref HEAD)"
readonly CURRENT_BRANCH
[[ "${CURRENT_BRANCH}" == "main" ]] || fail "must ship from main, not ${CURRENT_BRANCH}"
[[ -z "$(git status --porcelain)" ]] || fail "working tree is not clean"

RELEASE_TAG="$(git tag --points-at HEAD | head -n 1)"
readonly RELEASE_TAG
[[ -n "${RELEASE_TAG}" ]] || fail "HEAD carries no release tag; run 'just release-patch' first"
scripts/release-version.sh "${RELEASE_TAG}" >/dev/null

# A release commit must have been prepared directly from the current remote main. This catches a
# pull request merged after local preparation but before shipping.
git fetch origin main
if git rev-parse --verify HEAD^2 >/dev/null 2>&1; then
    fail "release HEAD must be a single-parent cargo-release commit"
fi
RELEASE_BASE="$(git rev-parse HEAD^)"
REMOTE_MAIN="$(git rev-parse refs/remotes/origin/main)"
readonly RELEASE_BASE REMOTE_MAIN
[[ "${RELEASE_BASE}" == "${REMOTE_MAIN}" ]] \
    || fail "release commit is not based on current origin/main; recreate the release from updated main"

if git ls-remote --exit-code --tags origin "refs/tags/${RELEASE_TAG}" >/dev/null 2>&1; then
    fail "release tag ${RELEASE_TAG} already exists on origin"
fi

readonly RELEASE_BRANCH="release/${RELEASE_TAG}"

# The main ruleset requires a pull request, so the release commit reaches main through a branch.
git push origin "HEAD:refs/heads/${RELEASE_BRANCH}"
RELEASE_PR_URL="$(
    gh pr create --base main --head "${RELEASE_BRANCH}" \
        --title "chore: Release ${RELEASE_TAG}" \
        --body "Version bump prepared by cargo-release. Merging publishes ${RELEASE_TAG}."
)"
readonly RELEASE_PR_URL
[[ "${RELEASE_PR_URL}" == https://* ]] || fail "release pull request creation returned no URL"
gh pr merge "${RELEASE_PR_URL}" --merge --delete-branch

# Tag the exact release-PR merge commit, not its cargo-release parent. The merge tree includes every
# change that reached main before the release PR and closes the race between preparation and merge.
RELEASE_MERGE_COMMIT="$(
    gh pr view "${RELEASE_PR_URL}" --json mergeCommit --jq '.mergeCommit.oid'
)"
readonly RELEASE_MERGE_COMMIT
[[ "${RELEASE_MERGE_COMMIT}" =~ ^[0-9a-f]{40}$ ]] \
    || fail "release pull request has no valid merge commit"
git fetch origin main
git cat-file -e "${RELEASE_MERGE_COMMIT}^{commit}" \
    || fail "release merge commit is unavailable locally"
git merge-base --is-ancestor HEAD "${RELEASE_MERGE_COMMIT}" \
    || fail "release merge commit does not contain the prepared release commit"

TAG_MESSAGE="$(git for-each-ref --format='%(contents:subject)' "refs/tags/${RELEASE_TAG}")"
readonly TAG_MESSAGE
git tag --force --annotate "${RELEASE_TAG}" "${RELEASE_MERGE_COMMIT}" \
    --message "${TAG_MESSAGE}"
git push origin "refs/tags/${RELEASE_TAG}"
git merge --ff-only refs/remotes/origin/main

printf 'released %s from %s\n' "${RELEASE_TAG}" "${RELEASE_MERGE_COMMIT}"
