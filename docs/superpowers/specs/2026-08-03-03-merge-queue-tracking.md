# Specification 3: Merge-Queue Tracking

## Goal

Track merge-group events and per-pull-request merge-queue attempts across restarts while preserving
bounded metrics and making no unsupported claim about GitHub dequeue reasons.

## Dependencies

Specification 2 must be complete.

## Supported events

After generic authentication and delivery claiming, process:

- `merge_group.checks_requested`
- `merge_group.destroyed`
- `pull_request.enqueued`
- `pull_request.dequeued`
- `pull_request.closed` when `pull_request.merged` is `true`

All other events continue through generic metrics only.

## Persistence

```sql
CREATE TABLE merge_queue_attempts (
    id INTEGER PRIMARY KEY,
    repository_id INTEGER NOT NULL REFERENCES repositories(id) ON DELETE CASCADE,
    pull_request_number INTEGER NOT NULL CHECK (pull_request_number > 0),
    enqueued_at TEXT NOT NULL,
    completed_at TEXT,
    outcome TEXT NOT NULL CHECK (
        outcome IN ('pending', 'succeeded', 'failed', 'cancelled', 'unknown')
    ),
    reason_code TEXT NOT NULL,
    CHECK (
        (outcome = 'pending' AND completed_at IS NULL) OR
        (outcome <> 'pending' AND completed_at IS NOT NULL)
    )
);

CREATE UNIQUE INDEX one_active_merge_queue_attempt
    ON merge_queue_attempts(repository_id, pull_request_number)
    WHERE completed_at IS NULL;

CREATE INDEX merge_queue_attempts_completed_at_idx
    ON merge_queue_attempts(completed_at);
```

The default completed-attempt retention is 90 days, configurable with
`GHE_MERGE_QUEUE_RETENTION_DAYS`. Pruning uses the same bounded scheduling model as delivery
retention.

## State transitions

- `enqueued` with no active attempt creates a pending attempt using the event timestamp after
  validating it; receipt time is used when the timestamp is absent or invalid.
- `enqueued` with an active attempt is idempotent and changes no state or outcome metrics.
- `dequeued` completes an active attempt as `unknown` with reason code `unclassified_dequeue`.
- `closed` with `merged == true` completes an active attempt as `succeeded` with reason code
  `pull_request_merged`.
- A completion event without an active attempt changes no persisted state and increments an
  internal transition-failure metric with normalized reason `missing_active_attempt`.
- A completion for an already completed attempt is a no-op.
- Concurrent transitions serialize in one SQLite transaction and rely on the partial unique index.

V1 deliberately does not guess whether a dequeue is a CI failure or user cancellation. The
`failed` and `cancelled` outcomes are reserved for a later classifier revision backed by documented
or observed stable inputs. Raw dequeue reasons are discarded and are neither persisted nor logged.

## Merge-group classification

`checks_requested` has reason `none`. A destroyed group maps its reason to `merged`, `dequeued`,
`invalidated`, or `other` through an exact fixed mapping. Unknown raw values are discarded.
Group-level `merged` is the authoritative success signal. Group events do not mutate pull-request
attempts because a merge group can contain multiple pull requests and webhook ordering is not a
reliable join mechanism.

## Metrics

```text
github_merge_group_events_total{repository,action,reason}
github_merge_queue_pr_outcomes_total{repository,outcome,reason}
github_merge_queue_attempt_duration_seconds{repository,outcome}
github_merge_queue_transition_failures_total{repository,reason}
```

The `repository` label is the authenticated canonical lowercase GitHub full name in
`owner/repository` form. All remaining labels use fixed enums. Outcome counters and durations update
only when a transaction changes an attempt from pending to completed. Negative or unreasonably large
durations are omitted and counted as a normalized transition failure rather than observed.

## Failure behavior

Queue processing begins after webhook authentication and claiming. A queue transaction failure does
not ask GitHub to redeliver and therefore preserves the existing `204` response. It emits a redacted
error, increments `github_webhook_processing_failures_total{stage="queue_state"}`, and records an
error event when specification 4 is present.

This is intentionally at-most-once queue processing at the delivery boundary: the already claimed
delivery will not be retried automatically after a state failure. Operational alerts must surface
such failures.

## Tests

- State-transition unit tests cover every event/state pair.
- Integration tests cover repeated, missing, out-of-order, and concurrent events.
- Restart tests prove pending attempts can be completed after reopening SQLite.
- Transaction rollback leaves attempts and outcome metrics unchanged.
- Dequeue reasons always map to `unknown/unclassified_dequeue` and raw values are discarded.
- Group reasons remain bounded, and repository labels contain only authenticated canonical names.
- Retention removes only completed attempts older than the configured threshold.
- Repository deletion cascades to queue attempts.

## Acceptance criteria

- Enqueued pull requests correlate with later outcomes across restarts.
- A successful merged pull request completes one active attempt exactly once during normal
  operation.
- Dequeued attempts are reported as unknown rather than mislabeled as failures.
- Group-level merge success remains separate and authoritative.
- Raw reason strings and unbounded identifiers other than authenticated canonical repository names
  never become metric labels or logs.
- Queue-state failures remain observable without changing authenticated webhook responses.
