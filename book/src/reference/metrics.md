# Metrics

All series are prefixed `github_` and carry bounded label sets. Unrecognized webhook event types
collapse to `event_type="other"` and unrecognized actions to `action="other"`, which is what keeps
cardinality fixed regardless of traffic.

## Repository-scoped

Carry a `repository` label: the authenticated canonical lowercase `owner/repository` name.
Requests that fail before authentication use the fixed value `unknown`.

| Metric | Labels |
| --- | --- |
| `github_webhook_requests_total` | `repository`, `result` |
| `github_webhook_events_total` | `repository`, `event_type`, `action` |
| `github_webhook_processing_duration_seconds` | `repository`, `result` |
| `github_webhook_request_body_bytes` | `repository` |
| `github_webhook_duplicates_total` | `repository` |
| `github_webhook_processing_failures_total` | `repository`, `stage` |
| `github_merge_group_events_total` | `repository`, `action`, `reason` |
| `github_merge_queue_pr_outcomes_total` | `repository`, `outcome`, `reason` |
| `github_merge_queue_attempt_duration_seconds` | `repository`, `outcome` |
| `github_merge_queue_transition_failures_total` | `repository`, `reason` |
| `github_workflow_job_steps` | `repository` |
| `github_workflow_job_trace_rejections_total` | `repository`, `reason` |

## Process-wide

Carry no `repository` label.

| Metric | Labels |
| --- | --- |
| `github_repository_configurations` | none |
| `github_telemetry_export_failures_total` | `signal`, `reason` |
| `github_telemetry_dropped_records_total` | `signal`, `reason` |

See [Remote telemetry export](telemetry.md) for the `signal`/`reason` vocabulary on the last two.

## Merge-group statistics

A newly claimed `merge_group.checks_requested` delivery increments
`github_merge_group_events_total{action="checks_requested",reason="none"}`. A newly claimed
`merge_group.destroyed` delivery uses `action="destroyed"` and maps the top-level `reason` field
exactly and case-sensitively to `merged`, `dequeued`, or `invalidated`; missing, non-string,
mixed-case, and unknown values map to `other`. Unsupported merge-group actions update only the
generic webhook event metric. Duplicate deliveries update only the webhook request and duplicate
counters.

`destroyed`/`merged` is the authoritative merge-group success statistic. These group metrics stay
separate from per-pull-request queue attempt outcomes below — a merge group can contain multiple
pull requests, and webhook ordering does not provide a reliable join, so merge-group deliveries
never create or mutate pull-request attempt rows. Raw reasons, group identifiers, and head SHAs are
discarded rather than logged or used as labels.

## Pull-request merge-queue attempts

For a newly claimed `pull_request` delivery, the authenticated repository's durable identifier and
positive `pull_request.number` select one attempt:

- `enqueued` creates a pending attempt only when none is active.
- `dequeued` completes an active attempt as `unknown`/`unclassified_dequeue`.
- `closed` completes it as `succeeded`/`pull_request_merged` only when `pull_request.merged` is
  exactly `true`.

An absent or false `merged` flag, and unsupported actions, do not mutate specialized state. Raw
dequeue reasons are discarded and never persisted, logged, or used as labels. Repeated `enqueued`
and already-completed replay are no-ops.

A completion with no active or completed attempt increments only
`github_merge_queue_transition_failures_total{reason="missing_active_attempt"}`. Outcome and
duration metrics update only after SQLite commits a pending-to-completed transition. Negative and
over-365-day durations are omitted and increment only the bounded `invalid_duration` transition
failure.

`github_webhook_processing_failures_total` uses the closed `stage` vocabulary `authentication`,
`delivery_claim`, `metrics`, `database`, `queue_state`, and `workflow_link`. The last two mark
enrichment-only paths that keep the authenticated `204 No Content` response: `queue_state` for
merge-queue persistence and `workflow_link` for pipeline-run link persistence, lookup, and export.
Every stage is published zero-valued at startup.

`github_workflow_job_trace_rejections_total` uses the closed `reason` vocabulary `too_many_steps`
and `too_many_jobs`: a completed job over `GHE_WORKFLOW_JOB_MAX_STEPS` emits no job trace, and a
run attempt over the fixed 256-job pipeline limit emits no pipeline-run summary trace. Both series
are published zero-valued at startup. See [Traces](traces.md).

SQLite state and in-memory Prometheus metrics are not one transaction: a crash after a durable
commit but before the corresponding metric update can undercount an outcome until the process
restarts. This is an intentional at-most-once boundary, not exactly-once metrics across crashes.
