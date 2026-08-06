# Named SQLite Child Spans

## Summary

Implemented bounded `sqlite.query` child spans for logical SQLite store operations.

## Changes

- Added one typed database span per repository, delivery, and merge-queue store operation.
- Moved `delivery.claim` tracing from the webhook call site into `DeliveryStore::claim`.
- Centralized database-operation instrumentation in telemetry trace policy.
- Disabled automatic OpenTelemetry target, location, thread, and inactivity span attributes so exported spans contain only explicitly approved bounded fields.
- Added focused OTLP tests for successful SQLite operation names and redacted SQLite failures.
- Serialized OTLP integration tests with a test-local async mutex to remove intermittent export-capture races.

## Verification

- `cargo test telemetry::otlp_test::sqlite --lib -- --nocapture`
- `cargo test telemetry::otlp_test::sqlite --lib && cargo test --test storage && cargo test --test delivery_storage && cargo test --test merge_queue_storage`
- `cargo test`
- `cargo build && cargo clippy -- -D warnings && cargo fmt --check`
