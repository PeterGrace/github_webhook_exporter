# Show the rate and label behind each job's cost

Date: 2026-11-13T22:30:00Z

## What changed

- Priced job spans and `ci cost recorded` logs now carry `ci.price.usd_per_minute`, the rate
  applied, so `ci.cost_usd = ci.billable_minutes × ci.price.usd_per_minute` can be checked from the
  span alone.
- They also carry `ci.price.label`, the `runners.labels` key that supplied the rate. It is a single
  string, so cost can be grouped by runner type in queries; the `cicd.runner.labels` array is kept
  but Sentry cannot group by array attributes. It is omitted when the default rate was used.
- Job spans and the log carry `cicd.runner.name`, the runner instance GitHub reports.
- A job that billed 0 minutes carries neither rate nor label, since no rate is looked up.
