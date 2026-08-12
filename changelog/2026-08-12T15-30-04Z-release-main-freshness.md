# Release main-freshness and tag-source hardening

## Problem

Version `v0.1.4` was prepared from a locally current `main` while another pull request was merging.
The release PR merged successfully, but cargo-release's pre-merge tag still named the stale version
commit. The published image reported `0.1.4` without the change already present on remote `main`.

## Changes

- Added `scripts/release-prepare.sh` to fetch `origin/main` and require exact local/remote equality
  before invoking cargo-release.
- Hardened `scripts/release-ship.sh` to fetch again and require the release commit's sole parent to
  equal current `origin/main` before creating remote release refs.
- Changed shipping to obtain the exact release PR merge commit from GitHub, verify the prepared
  release commit is its ancestor, and move the local annotated tag to that merge commit before
  publishing it.
- Reject release tags that already exist remotely.
- Added hermetic Git-based release-flow tests for stale preparation, stale shipping, and a main
  change landing between the shipping guard and release merge.
- Added the release-flow test to the standard CI validation job and updated the release how-to.

## Result

A release now fails before remote mutation when its prepared base is stale. A change that lands in
the remaining race window is included because the published tag names the release PR merge tree,
not its older cargo-release parent.
