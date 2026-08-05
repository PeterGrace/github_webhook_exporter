# Pull-Request Merge-Queue Attempt Tracking

## Added

- Returned the typed durable repository identifier after successful repository-specific HMAC
  authentication without exposing identity or secret material in errors or logs.
- Added authenticated processing for `pull_request.enqueued`, `pull_request.dequeued`, and merged
  `pull_request.closed` deliveries after the durable new-delivery claim.
- Correlated pending and completed attempts across SQLite restarts and concurrent webhook requests.
- Recorded bounded outcome, duration, missing-attempt, invalid-duration, and queue-state failure
  metrics only at their specified transition boundaries.
- Added signed-router coverage for dequeue and merged outcomes, replay and duplicate idempotency,
  receipt-time fallback, malformed projections, missing attempts, invalid durations, restart,
  concurrency, and redacted queue-state failures.

## Security and reliability

- Discarded raw dequeue reasons instead of persisting, logging, or labeling them.
- Preserved `204 No Content` after a claimed delivery encounters queue persistence failure while
  emitting one redacted correlated local error and incrementing the bounded `queue_state` stage.
- Documented the at-most-once delivery boundary and the crash boundary between committed SQLite
  queue state and in-memory Prometheus metrics.
