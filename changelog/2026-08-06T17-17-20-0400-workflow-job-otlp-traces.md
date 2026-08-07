# Completed workflow-job OTLP traces

## Implementation

- Added an authenticated, bounded projection for newly claimed `workflow_job.completed` payloads.
- Added a direct SDK historical emitter with one independent `github.workflow.job` root and direct
  `github.workflow.step` children using explicit reported or fallback timestamps.
- Normalized conclusions, statuses, identifiers, pull-request arrays, and sanitized display names
  to the approved fixed attribute policy.
- Preserved at-most-once delivery admission and the existing non-blocking bounded trace queue.

## Privacy and failure isolation

- Kept approved workflow names and identifiers in spans only.
- Excluded commands, output and logs, actors, URLs, payload fragments, secrets, signatures, headers,
  and raw unknown conclusions from traces, OTLP logs, structured stderr, and Prometheus exposition.
- Added integrated in-process OTLP coverage for cross-signal privacy and centralized attribute/event
  allowlists.
- Added collector-unavailability coverage for the unchanged `204 No Content` response, readiness,
  generic metrics, and empty merge-queue state, using runtime failure counters without requiring a
  successful force flush against the unavailable endpoint.
- Documented completed-only admission, trace identity, timing, status, identifiers, name bounds,
  privacy, and collector-failure behavior in `docs/operations.md`.

## Final validation pending Task 7

Task 7 must run and record results for these final gates; this changelog does not yet claim they
pass:

1. `just fmt`
2. `cargo build`
3. `cargo clippy --all-targets -- -D warnings`
4. `just test`
5. `cargo doc --no-deps`
