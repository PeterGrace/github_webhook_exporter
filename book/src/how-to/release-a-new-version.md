# How to release a new version

This takes a clean, current `main` to a published version tag. Releases go through
[`cargo-release`](https://github.com/crate-ci/cargo-release) and land via a pull request, because
a repository ruleset forbids direct pushes to `main`.

## Before you start

- You're on `main` in the primary checkout, up to date with `origin/main`, with a clean working
  tree.
- `cargo-release` and the `gh` CLI are installed, and `gh auth status` succeeds.
- The full check suite passes: `just fmt`, `just test`, `just helm-static`.

## 1. Prepare the release

```bash
just release-patch
```

The preparation script first fetches `origin/main` and stops unless local `main` is exactly current.
It then bumps the crate version; rewrites `version` and `appVersion` in
`charts/github-webhook-exporter/Chart.yaml` to match; rewrites the pinned version strings in
`README.md`, `charts/github-webhook-exporter/README.md`, and
[Release and packaging](../reference/release-and-packaging.md) to match; commits the result; and
creates the annotated `vMAJOR.MINOR.PATCH` tag. Nothing is pushed yet.

For a minor or major release, run `cargo-release` directly with the same flags:

```bash
cargo release --no-publish --no-verify --no-push minor --execute
```

Inspect what you're about to publish before continuing:

```bash
git show --stat HEAD
git tag --points-at HEAD
```

## 2. Ship it

```bash
just release-ship
```

This fetches `origin/main` again and verifies that the release commit's parent is still the current
remote tip. It then pushes the release commit to `release/<tag>`, opens a pull request against
`main`, and merges it. After GitHub reports the exact release-PR merge commit, the script moves the
local annotated tag from the cargo-release commit to that merge commit and publishes the tag. It
finishes by fast-forwarding local `main`.

The script refuses to run unless you're on `main` with a clean tree, `HEAD` carries a tag, the
release commit has exactly one parent equal to current `origin/main`, and the tag does not already
exist remotely. It validates the tag against the Cargo and chart versions via
`scripts/release-version.sh` before changing any remote refs.

Pushing the tag triggers the tag job in `.github/workflows/helm-package-ci.yml`, which packages
and publishes the image and chart — see
[Release and packaging](../reference/release-and-packaging.md) for exactly what that job will and
won't overwrite.

## 3. Confirm

```bash
gh run list --workflow helm-package-ci.yml --limit 3
```

## If you need to abandon a prepared release

Before `just release-ship`, the release exists only locally:

```bash
git tag -d vX.Y.Z
git reset --hard origin/main
```

## If the push is rejected with GH013

```text
remote: error: GH013: Repository rule violations found for refs/heads/main.
remote: - Changes must be made through a pull request.
```

You pushed the release commit straight at `main`. The ruleset requires a pull request and grants
no bypass, including for repository admins — that's also why the tag appears rejected when
`cargo-release` pushes branch and tag atomically; the rejected branch update takes the tag down
with it. Run `just release-ship` instead of pushing by hand; if the release commit and tag already
exist locally, it picks them up as-is.

## Why the tag names the release PR merge commit

Tags aren't protected — only `main` is gated — so the tag goes last. `cargo-release` initially
creates it on the local version-bump commit, but that commit can become stale if another pull
request lands while the release is being prepared. GitHub's merge commit combines the current base
with the release commit, so `release-ship.sh` retargets the annotation to the exact merge commit
reported for the release PR before pushing it. The published image and chart therefore come from
the tree that actually landed on `main`, including changes merged before the release PR.

The ruleset permits only the `merge` method, so the cargo-release commit remains an ancestor of the
tagged merge commit. The script verifies that ancestry before publication.
