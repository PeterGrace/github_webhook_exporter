# Group span cost by pull request, and attribute merge-queue runs

Date: 2026-11-14T00:30:00Z

## What changed

- Job and pipeline roots carry `ci.pull_request.number`, a single integer, when their pull
  requests name exactly one distinct PR. `github.pull_request.number` stays an array, but Sentry
  doesn't index array attributes, so span cost could not be grouped by PR before.
- A `merge_group` run is attributed to the PR it is merging, read from its queue branch
  `gh-readonly-queue/<base>/pr-<number>-<head sha>`. Before, merge-queue CI had no PR.
- The `ci cost recorded` log carries `vcs.ref.head.revision`, so log cost can be grouped by commit.
