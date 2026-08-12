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
- Repository-wide `cargo build`, `cargo clippy -- -D warnings`, and `cargo test` currently surface unrelated pre-existing failures outside this task (`src/telemetry.rs` Sentry client builder API usage and `retention::tests::prune_failure_is_redacted_and_carries_a_correlation_id`).
