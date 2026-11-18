# Price per-run vendor charges from check suites and pull-request reviews

Date: 2026-11-17T22:00:00Z

## What changed

- A newly claimed `check_suite` delivery with action `completed` whose app slug matches a
  `costs.vendors` entry's `match.check_suite_app`, or a `pull_request_review` delivery with action
  `submitted` whose reviewer login matches `match.review_login`, now counts as one vendor run
  priced at that vendor's `usd_per_run`. Matching ignores ASCII case. Other actions, unmatched
  deliveries, redeliveries and deployments without `GHE_COSTS_JSON` record nothing. Each matching
  delivery is one charge, so several reviews from one bot on a pull request are several runs. A
  matched delivery with a missing or malformed pull request number or head SHA is still priced,
  just unattributed.
- Each run exports one instantaneous, parentless `ci.vendor.run` span and writes one
  `ci cost recorded` info log, with `ci.source=vendor`, `ci.vendor`, `ci.vendor.trigger`
  (`check_suite` or `pull_request_review`), `ci.cost_usd`, `ci.price_table.version`, the
  repository, the head revision and, when attributed, the pull request. The log is written even
  when trace export is off.
- A check suite is attributed to a pull request only when `check_suite.pull_requests` names exactly
  one distinct number. Otherwise its span and log carry no pull request and nothing is stored.
- Attributed runs are stored in the new `ci_vendor_runs` table (migration `202611180001`), keyed by
  repository and pull request, and pruned on the delivery retention cutoff after in-job usage
  entries.
- A storage failure keeps the `204 No Content` response, increments the new
  `github_webhook_processing_failures_total{stage="vendor_run"}` series, and logs one redacted
  `vendor-run persistence failed` error.
- `GHE_COSTS_JSON` now fails startup when a `costs.vendors` name is longer than 128 characters or
  contains a control character, because vendor names are exported and stored unchanged. The Helm
  chart schema rejects the same names.
- New span attribute keys `ci.vendor` and `ci.vendor.trigger`, and new database operations
  `vendor_run.record` and `vendor_run.prune`.

## Operator action

Subscribe the repository webhook to **Check suites** and **Pull request reviews** for vendors
matched by app slug and reviewer login respectively. Without those events GitHub sends nothing to
match.
