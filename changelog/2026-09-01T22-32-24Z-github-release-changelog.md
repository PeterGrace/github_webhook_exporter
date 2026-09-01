# GitHub release publication with a landed-commit changelog

## Summary

A stable tag now mints a real GitHub release once the immutable image and Helm chart are published.
The release carries generated notes describing every commit that landed since the previous stable
tag, and attaches the already-validated chart archive.

## Changes

- Added `scripts/release-changelog.sh`, which renders release notes for a version. It selects the
  newest stable tag the release tag actually descends from as the comparison base, walks
  `previous..release` with `--no-merges`, drops the `cargo-release` version-bump commit, and groups
  the remaining subjects by conventional-commit type into Breaking changes, Features, Fixes,
  Performance, Refactoring, Documentation, Testing, Build, Continuous integration, Chores, Style,
  Reverts, and Other changes. Scopes render inline; a `!` marker promotes an entry to Breaking
  changes. The notes open with the pinned `docker pull` and `helm install` commands and close with a
  compare link, falling back to a commits link for a repository's first release.
- Added `scripts/release-announce.sh`, which creates the GitHub release through `gh`. It validates
  the version and the chart archive name, refuses to invent a tag (`--verify-tag`), and treats an
  existing release page as complete so a chart-only recovery rerun stays idempotent. Inspection and
  creation failures fail closed.
- Added `scripts/release-changelog-test.sh` and `scripts/release-announce-test.sh`. The changelog
  test builds a fixture history covering scopes, breaking markers, merge commits, non-conventional
  subjects, a prerelease tag, and the release bump, then asserts section grouping, section order,
  exclusions, the install block, the compare and commits links, and every rejected input. The
  announcement test drives the script against a fake `gh` and asserts the exact command contract,
  the generated notes body, the skip-on-existing path, both fail-closed paths, and input rejection.
- Added the `release-notes-test` recipe to `justfile`, running both new test scripts.
- `.github/workflows/helm-package-ci.yml`: the validate job runs `just release-notes-test` on every
  pull request and push; the `publish-release` job checks out with `fetch-depth: 0` so tags and
  history are available to the changelog, raises its own permission to `contents: write`, and runs
  `scripts/release-announce.sh` as its final step, after `scripts/release-publish.sh` succeeds.
- `scripts/github-actions-test.sh`: extended the workflow contract to cover the new validation step,
  the `fetch-depth: 0` checkout, the `contents: write` publish permission, the announcement step,
  and the new documentation fragments.
- Documented the behavior in `book/src/reference/release-and-packaging.md` (new "Release notes"
  section) and `book/src/how-to/release-a-new-version.md` (previewing notes locally, confirming with
  `gh release view`, and the safe rerun path).

## Design notes

- The announcement runs after publication, not before, so a release page can never advertise an
  image or chart that failed to reach GHCR.
- An existing release page is never rewritten. That is deliberately weaker than the registry
  overwrite guard: a release page is a description of artifacts that are themselves already
  immutable, so leaving it alone keeps recovery reruns idempotent rather than failing them closed.
- `contents: write` is scoped to the tag job. Pull request and `main` validation runs keep
  `contents: read`.

## Verification

- `just workflow-test`
- `just release-notes-test`
- `just release-flow-test`
- `just fmt`
- `shellcheck` over every tracked shell file
- `mdbook build book`
- `scripts/release-changelog.sh 0.1.10`, `0.1.8`, and `0.1.1` against real repository history
