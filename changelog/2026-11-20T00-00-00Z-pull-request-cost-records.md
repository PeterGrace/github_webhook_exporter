# Finalize one cost record per pull request

Date: 2026-11-20T00:00:00Z

## What changed

- When a `costs` table or `ingest.oidc` is configured, each pull request gets one final cost
  record, built from every run total stored against it and every vendor run attributed to it.
  It is exported once, as a parentless `ci.pr.cost` span and a `ci pr cost finalized` log, with
  `github.pr_outcome`, `ci.pr.commits`, `ci.pr.runs`, `ci.pr.reruns`, `ci.pr.vendor_runs`,
  `ci.pr.open_hours`, `ci.cost.total_usd`, one `ci.cost.<source>_usd` per source (including each
  vendor), `ci.pr.finalized_by` and `ci.pr.partial`. The log carries the breakdown as
  `ci.cost.by_source`.
- A fourth background task polls every 60 seconds and finalizes a pull request when the first rule
  applies:
  - `post_merge_run`: merged, and no `push` run on its merge commit totalled for
    `GHE_PR_COST_SETTLE_SECONDS` (default `1800`; Helm `application.prCostSettleSeconds`);
  - `timeout`: merged, with no `push` run on its merge commit within
    `GHE_PR_COST_POST_MERGE_TIMEOUT_SECONDS` (default `21600`; Helm
    `application.prCostPostMergeTimeoutSeconds`);
  - `closed_grace`: closed without merging for `GHE_PR_COST_CLOSED_GRACE_SECONDS` (default `3600`;
    Helm `application.prCostClosedGraceSeconds`);
  - `max_age`: no activity for `GHE_PR_COST_RETENTION_DAYS` (default `90`; Helm
    `retention.pullRequestCostDays`).
  The first three wait while any of the pull request's runs is still inside its own run grace
  period.
- `pull_request` deliveries now record each pull request's `created_at`, and on `closed` whether it
  merged, its `merge_commit_sha` and `closed_at`; `reopened` makes it open again until finalized.
- Each finalized run total now also stores its head commit and its cost per source, so a pull
  request's record can split its cost by source after the run's job links and usage entries are
  pruned.
- A run total or vendor run that arrives after its pull request's record is final is stored but
  not counted, and logs a `ci cost after finalize` warning naming the pull request.
- `ci.pr.partial` is `true` for a pull request created before run totals began, or whose creation
  time is unknown. When run totals began is written once.
- Vendor runs and per-pull-request run totals now use the `GHE_PR_COST_RETENTION_DAYS` cutoff
  instead of the delivery cutoff, and are kept regardless of age while their pull request's record
  is pending. A pending record is never pruned.
- Migration `202611200001` adds `pull_request_run_costs.head_sha` and the tables
  `pull_request_run_cost_sources`, `pull_request_costs` and `pull_request_cost_rollup`. It fills
  the head commit and per-source split of existing run totals from the run schedules, job links and
  usage entries retention still holds, and dates the start of run totals from the earliest stored
  schedule. Every pull request with a stored run total or vendor run starts with a pending record,
  so it is finalized and its stored costs are kept until then. It also adds two partial indexes on
  pending `run_cost_finalizations`, by pull request and by head commit, for the finalizer's
  pending-run check.
- Finalization evaluates its rule again inside its write transaction, so a pull request reopened,
  or given a new pending run, between the poll and the write is left for a later poll.

## Operator action

Back up the SQLite volume before upgrading. Migration `202611200001` is forward-only, so roll back
by restoring that backup.

Pull requests that already had stored costs at upgrade start pending with no creation time. One
that sees a later `pull_request` delivery records its creation time, and its record is partial
only if it was created before run totals began. One that already closed before the upgrade gets no
close event, so it is finalized with `max_age` and `github.pr_outcome=open` once it has been
inactive for `GHE_PR_COST_RETENTION_DAYS`, and is partial.
