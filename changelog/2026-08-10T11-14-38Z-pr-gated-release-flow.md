# PR-gated release flow

## Problem

`just release-patch` failed at the push step:

```
remote: error: GH013: Repository rule violations found for refs/heads/main.
remote: - Changes must be made through a pull request.
 ! [remote rejected] main -> main (push declined due to repository rule violations)
 ! [remote rejected] v0.1.1 -> v0.1.1 (atomic transaction failed)
```

The repository ruleset `main` (id 20396387) targets `~DEFAULT_BRANCH` and carries a
`pull_request` rule with `bypass_actors: []` and `current_user_can_bypass: "never"`.
Direct pushes to `main` are therefore impossible, including for repository admins.

No ruleset targets tags. The tag was rejected only as collateral damage:
`cargo release` pushes the branch and the tag in a single atomic push, so the
declined `main` update rolled back the `v0.1.1` update alongside it.

## Change

Split the release into a local preparation step and a PR-gated shipping step.

- `just release-patch` now passes `--no-push`, leaving the version-bump commit and
  the annotated tag in the local repository only.
- `just release-ship` runs `scripts/release-ship.sh`, which pushes the release
  commit to `release/<tag>`, opens a pull request against `main`, merges it, and
  then pushes the tag on its own.

The tag is pushed after the merge so it always names a commit reachable from
`main`. Because the ruleset permits only the `merge` method, the release commit is
preserved as a parent of the merge commit rather than rewritten, which keeps the
tag valid.

`scripts/release-ship.sh` refuses to run unless it is on `main`, the working tree
is clean, and `HEAD` carries a tag; the tag is then validated against the Cargo and
Helm chart versions by the existing `scripts/release-version.sh`.

Tag pushes trigger the `v[0-9]+.[0-9]+.[0-9]+` branch of `helm-package-ci.yml`, so
packaging still runs from the tag exactly as before.

## Files

- `justfile` — `release-patch` gains `--no-push`; new `release-ship` recipe.
- `scripts/release-ship.sh` — new.

## Follow-up

This branch is based on the `v0.1.1` release commit, so merging it lands both the
version bump and the release tooling. The tag is published afterwards with
`git push origin v0.1.1`.
