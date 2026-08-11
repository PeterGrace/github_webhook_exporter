# How to release a new version

This takes a clean `main` to a published version tag. Releases go through
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

This bumps the crate version; rewrites `version` and `appVersion` in
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

This pushes the release commit to `release/<tag>`, opens a pull request against `main`, merges it,
and pushes the tag on its own once the merge lands. It finishes by fast-forwarding your local
`main` onto the merge commit.

The script refuses to run unless you're on `main` with a clean tree and `HEAD` carries a tag, and
it validates that tag against the Cargo and chart versions via `scripts/release-version.sh`.

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

## Why the tag is pushed after the merge

Tags aren't protected — only `main` is gated — so the tag goes last, once it names a commit
already reachable from `main`. The ruleset permits only the `merge` method, so the release commit
survives as a parent of the merge commit rather than being rewritten, and the tag stays valid. Full
rationale: [`changelog/2026-08-10T11-14-38Z-pr-gated-release-flow.md`](https://github.com/PeterGrace/github_webhook_exporter/blob/main/changelog/2026-08-10T11-14-38Z-pr-gated-release-flow.md).
