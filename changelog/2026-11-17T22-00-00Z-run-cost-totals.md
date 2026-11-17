# Total each workflow run's cost after it completes

Date: 2026-11-17T22:00:00Z

## What changed

- When a `costs` table or `ingest.oidc` is configured, a `workflow_run.completed` delivery
  schedules its run attempt to be totalled after `GHE_RUN_GRACE_SECONDS` (default `300`, range
  `1..=3600`; Helm value `application.runGraceSeconds`). The delivery still returns
  `204 No Content` at once.
- A background finalizer, run alongside retention and the required-check refresher, polls SQLite
  every 15 seconds and totals due attempts. Pending attempts are stored in SQLite, so a restart
  during the grace period loses nothing. A redelivered `completed` for the same attempt schedules
  nothing more, and each re-run attempt is totalled on its own.
- Each total exports one `ci.run.cost` span and one `ci run cost finalized` log in the run's
  derived pipeline trace, with `ci.cost.total_usd`, per-source `ci.cost.<source>_usd`,
  per-label `ci.minutes.<label>`, `ci.usage.entries`, `cicd.pipeline.result`,
  `github.pr_resolution` and the run's workflow, event, identifiers and pull request. The log
  carries the breakdowns as `ci.cost.by_source` and `ci.minutes.by_label` lists, since log field
  names are fixed. Each breakdown is capped at 16 keys. Minutes no price table priced count under
  `unpriced`, and priced minutes stored before runner labels were recorded count under
  `unlabelled`.
- The pull request comes from webhook state only: the run's `pull_requests`, a merge-queue branch,
  `PR #<N>` in a comment-triggered run's name, or a new commit-to-pull-request map filled from
  `pull_request` deliveries (head commit on opened, synchronize and reopened; merge commit on
  closed). A run with none logs a `ci run unattributed` warning and still emits its total.
- Totals attributed to a pull request are stored per pull request, run and attempt for later
  aggregation. An attempt is stored under at most one pull request.
- A job or usage post that arrives after its run was totalled still logs `ci cost recorded`, is
  left out of the stored total, and logs a `ci cost after finalize` warning.
- Job billing is now stored even when trace export is off, and for jobs rejected by
  `GHE_WORKFLOW_JOB_MAX_STEPS`, so job costs count toward run totals either way. Rejected jobs
  still emit no job trace but now log `ci cost recorded` when priced. Job links also record the
  normalized price-table label each job was billed under.
- A run total that fails to write is retried after a delay that doubles from 30 seconds up to one
  hour, and a pending row that no longer parses is set aside rather than retried.
- Migration `202611180002` rebuilds `workflow_job_links` (nullable trace and span IDs, new
  `runner_label`) and adds `run_cost_finalizations`, `pull_request_run_costs` and
  `pull_request_commits`. All three new tables are pruned on the delivery-retention cutoff.
