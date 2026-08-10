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

readonly RELEASE_BRANCH="release/${RELEASE_TAG}"

# The main ruleset requires a pull request, so the release commit reaches main
# through a branch; only the tag is pushed directly afterwards.
git push origin "HEAD:refs/heads/${RELEASE_BRANCH}"
gh pr create --base main --head "${RELEASE_BRANCH}" \
    --title "chore: Release ${RELEASE_TAG}" \
    --body "Version bump prepared by cargo-release. Merging publishes ${RELEASE_TAG}."
gh pr merge "${RELEASE_BRANCH}" --merge --delete-branch
git push origin "refs/tags/${RELEASE_TAG}"
git fetch origin main
git merge --ff-only origin/main

printf 'released %s\n' "${RELEASE_TAG}"
