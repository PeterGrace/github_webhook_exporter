# Independent Retention Roots

## Summary

- Instrumented each scheduled retention pass with an independent `retention.run` root span.
- Added bounded root outcomes for successful, cancelled, and failed retention passes.
- Kept delivery and merge-queue prune SQLite spans as descendants without adding cutoff values, raw errors, correlation IDs, or identifiers to traces.
- Added OTLP coverage for completed, cancelled-between-workloads, invalid-cutoff, and one-store-failure retention passes.

## Validation

- `cargo test telemetry::otlp_test::retention --lib -- --nocapture`
- `cargo test retention --lib && cargo test telemetry::otlp_test::retention --lib`
