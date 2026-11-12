# Price completed workflow jobs and attribute runs to pull requests

Date: 2026-11-11T21:34:38Z

## What changed

- Completed `workflow_job` deliveries now project the runner labels (at most 16, sanitized) and
  runner group, and compute billable minutes: run time rounded up to a whole minute, or 0 for a
  skipped job, a job that never got a runner (no positive `runner_id`, as when cancelled while
  queued), or one without valid reported timing.
- With a `costs` table configured, each job is priced at billable minutes times the runner rate
  from `CostsConfig::runner_rate`. A job that billed 0 minutes is priced at 0 without a rate
  lookup, so skipped jobs never log `ci price missing`.
- Job root spans carry `cicd.runner.labels`, `cicd.runner.group`, `ci.billable_minutes`, and
  `github.workflow`. Priced jobs also carry `ci.source` (`github_actions`), `ci.cost_usd`,
  `ci.price_table.version`, and `ci.price.fell_back`.
- Each priced job writes one parentless `ci cost recorded` info log with the same cost fields plus
  the run, job, workflow, event and PR identifiers. It is a documented exception to the rule that
  workflow identifiers stay out of logs, and it exists only when `costs` is configured.
- `workflow_run` context now records the workflow file (`github.workflow_file`) and one
  attributed pull request: the single distinct number in `pull_requests`, or, for
  `issue_comment`, `pull_request_review` and `pull_request_review_comment` runs, the first
  `PR #<number>` in the run name that starts it or follows a non-alphanumeric character. Job roots whose own PR list is empty (GitHub sends none on
  `workflow_job`) and pipeline roots whose run reported none use that PR.
- The stored workflow event vocabulary adds `pull_request_target`, `pull_request_review`,
  `pull_request_review_comment`, `issue_comment`, `schedule`, and `workflow_dispatch`.
- Migration `202611110001` rebuilds `workflow_run_contexts` (SQLite cannot change a `CHECK`
  constraint) with `pull_request_number` and `workflow_file`, and adds `billable_minutes`,
  `cost_usd`, and `price_table_version` to `workflow_job_links`. Existing rows are kept.

## Why

Issue #127 (release R1 of CI cost attribution). Runner minutes are the largest CI cost and the
only one the exporter can price from webhooks alone. The PR and workflow-file attribution is what
per-PR and per-workflow cost queries group by.

## Decisions

- **Existing attribute names are reused.** Event, branches, head SHA and conclusion were already
  on job and pipeline spans as `github.workflow.event`, `github.workflow.source_branch`,
  `vcs.ref.head.revision` and `github.workflow.conclusion`, so no duplicates were added.
  `github.workflow` is the one deliberate repeat of `cicd.pipeline.name`, because in-job usage
  spans will group by it.
- **The run name is parsed only for comment-triggered events.** Push runs on the default branch
  have squash-merge titles such as `fix: x (#9088)` that would otherwise match.
- **`cost_usd` is stored as `REAL`.** Run totals sum at most a few hundred job costs, so `f64`
  rounding stays many orders of magnitude below a cent, and spans and logs carry `f64` anyway.
- **No dedupe for `ci price missing`.** One warning per priced job is bounded, and
  `ci.price.fell_back` on the span lets fallbacks be counted without searching logs.

## Out of scope

Runner labels and group are not persisted, because nothing reads them back yet. Run and PR totals
(#130, #131), the usage endpoint (#128) and vendor counting (#129) are separate issues.
