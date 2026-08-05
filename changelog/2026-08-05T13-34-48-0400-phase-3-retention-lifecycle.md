# Phase 3 Retention and Lifecycle Completion

Issue #26 completes Phase 3 operational retention and lifecycle behavior.

## Changes

- Added validated `GHE_MERGE_QUEUE_RETENTION_DAYS` configuration with a 90-day default and redacted rejection of zero, malformed, non-Unicode, and overflowing values.
- Generalized the existing retention runner to prune delivery claims and completed merge-queue attempts on the shared delivery-prune cadence.
- Preserved one skipped-tick ticker, fixed cutoffs per pass, 1,000-row SQLite operation bounds, pending queue attempts, and fresh completed attempts.
- Added normalized workload outcomes and opaque failure correlation IDs without SQL, row, repository, pull-request, timestamp, or payload details.
- Wired both retention workloads into the existing watch-based cancellation signal and shared HTTP shutdown timeout.
- Strengthened lifecycle, restart, duplicate-delivery, bounded-pruning, failure-recovery, and redaction regressions.
- Documented queue retention, authoritative merge-group success, unknown dequeue classification, and at-most-once crash boundaries in `docs/operations.md`.
