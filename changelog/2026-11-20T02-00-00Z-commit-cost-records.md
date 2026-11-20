# Finalize one cost record per commit

Date: 2026-11-20T02:00:00Z

## What changed

- When a `costs` table or `ingest.oidc` is configured, each commit gets one final cost record, so
  cost percentiles can be charted per commit as well as per run and per pull request. It is built
  from the run totals of the runs the commit triggered (`pull_request`, `pull_request_target`,
  `push` and `merge_group` runs on it, every attempt counted once) and the vendor runs on it.
  Comment, review, schedule, dispatch and chained `workflow_run` runs are left out.
- The record is exported once, as a parentless `ci.commit.cost` span and a
  `ci commit cost finalized` log, with `vcs.ref.head.revision`, `ci.pull_request.number` when its
  runs were attributed to a pull request, `ci.commit.trigger` (`merge_group`, `push` or
  `pull_request`), `ci.commit.runs`, `ci.commit.reruns`, `ci.commit.vendor_runs`,
  `ci.commit.partial`, `ci.cost.total_usd` and one `ci.cost.<source>_usd` per source.
- A fifth background task polls every 60 seconds and finalizes a commit once none of its
  commit-triggered runs is pending and nothing has reached it for `GHE_COMMIT_COST_SETTLE_SECONDS`
  (default `21600`; Helm `application.commitCostSettleSeconds`), or once it has had no activity
  for `GHE_PR_COST_RETENTION_DAYS`. The rule is evaluated again inside the write transaction.
- A run total or vendor run that arrives after its commit's record is final is not counted, and
  logs a `ci commit cost after finalize` warning.
- The late-cost type shared by the pull-request and commit warnings is now `LateCost`.
- A commit's run totals are deleted when its record is final; its state is pruned on the
  `GHE_PR_COST_RETENTION_DAYS` cutoff, and vendor runs are kept while their commit's record is
  pending.
- Migration `202611200003` adds `commit_run_costs`, `commit_run_cost_sources`, `commit_costs` and
  `commit_costs_before_rollup`, and an index on `ci_vendor_runs` by head commit. It marks every
  commit with a commit-triggered run total already finalized, so a record for one of those commits
  is `ci.commit.partial=true`.

## Operator action

Back up the SQLite volume before upgrading. Migration `202611200003` is forward-only, so roll back
by restoring that backup.

Filter on `ci.commit.partial:false` for percentiles: commits whose runs straddle the upgrade are
partial, and the first records appear six hours after the first runs finish on the new version.
