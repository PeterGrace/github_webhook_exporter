# How to cut a release

This guide takes a clean `main` to a published version tag. Releases are driven by
[`cargo-release`](https://github.com/crate-ci/cargo-release) and land through a pull
request, because the `main` branch ruleset forbids direct pushes.

## Before you start

- You are on `main` in the primary checkout, up to date with `origin/main`, with a
  clean working tree.
- `cargo-release` and the `gh` CLI are installed, and `gh auth status` succeeds.
- The full check suite passes:

  ```
  just fmt
  just test
  just helm-static
  ```

## 1. Prepare the release

```
just release-patch
```

This bumps the crate version, rewrites `version` and `appVersion` in
`charts/github-webhook-exporter/Chart.yaml` to match, commits the result, and
creates the annotated `vMAJOR.MINOR.PATCH` tag. Nothing is pushed.

For a minor or major release, run `cargo release` directly with the same flags:

```
cargo release --no-publish --no-verify --no-push minor --execute
```

Inspect what you are about to publish before continuing:

```
git show --stat HEAD
git tag --points-at HEAD
```

## 2. Ship it

```
just release-ship
```

This pushes the release commit to `release/<tag>`, opens a pull request against
`main`, merges it, and then pushes the tag on its own. It finishes by
fast-forwarding your local `main` onto the merge commit.

The script refuses to run unless you are on `main` with a clean tree and `HEAD`
carries a tag, and it validates that tag against the Cargo and chart versions via
`scripts/release-version.sh`.

Pushing the tag triggers the tag job in `.github/workflows/helm-package-ci.yml`,
which packages and publishes the chart and image.

## 3. Confirm

```
gh run list --workflow helm-package-ci.yml --limit 3
```

## If you need to abandon a prepared release

Before `just release-ship`, the release exists only locally. Undo it with:

```
git tag -d vX.Y.Z
git reset --hard origin/main
```

## If the push is rejected with GH013

```
remote: error: GH013: Repository rule violations found for refs/heads/main.
remote: - Changes must be made through a pull request.
```

You pushed the release commit straight at `main`. The `main` ruleset requires a
pull request and grants no bypass, so this fails for everyone including repository
admins. When `cargo-release` pushes the branch and tag in a single atomic push, the
rejected branch update takes the tag down with it, which is why the tag also
appears as rejected.

Run `just release-ship` instead of pushing by hand. If the release commit and tag
already exist locally, `just release-ship` will pick them up as-is.

## Why the tag is pushed after the merge

Tags are not protected, so a tag can be pushed directly; only `main` is gated. The
tag goes last so it always names a commit already reachable from `main`. The
ruleset permits only the `merge` method, so the release commit survives as a parent
of the merge commit rather than being rewritten, and the tag stays valid.

Full rationale: [`changelog/2026-08-10T11-14-38Z-pr-gated-release-flow.md`](changelog/2026-08-10T11-14-38Z-pr-gated-release-flow.md).
