# 2026-08-12T19:34:33Z OTLP workflow exception events task 1

## Summary
- Added regression coverage for canonical OpenTelemetry `exception` span events on historical workflow job and step spans.
- Exporter tests now verify child failure/timeout events, job fallback behavior, event timestamps, and non-failure absence.
- OTLP serialization tests now allowlist `exception` events and assert exact exported event payloads.
- `WorkflowTraceEmitter` now records canonical exception events before optional Sentry reporting.
- `SyntheticWorkflowError` now exposes shared span-event attributes for OpenTelemetry and Sentry reuse.

## Validation
- Focused RED commands failed before the production change because workflow spans exported no `exception` events.
- Focused GREEN commands now pass for workflow emitter coverage, workflow error coverage, and OTLP serialization coverage.

## Notes
- The repository-wide build, Clippy, and test failures observed during this intermediate task were transient. The task-3 validation pass resolved the Clippy/test issues, and the PR #80 final-review fix wave corrected the production Sentry client construction path.
