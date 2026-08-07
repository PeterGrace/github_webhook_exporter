# 2026-08-07T00:44:50Z - Explicit-time workflow trace emitter

## Summary
- Added `WorkflowTraceEmitter` for explicit-time historical GitHub Actions job and step spans.
- Wired the emitter through `TelemetryRuntime`, `AppState`, production startup, and `WebhookTraceFixture`.
- Kept `AppState::new` unchanged and defaulted the emitter to disabled.
- Updated OTLP assertions to match decimal-string workflow pull-request identifiers.

## Files
- `src/telemetry/workflow.rs`
- `src/telemetry.rs`
- `src/app.rs`
- `src/main.rs`
- `src/telemetry/otlp_test.rs`

## Verification
- `cargo test telemetry::workflow::tests --lib`
- `cargo test telemetry::otlp_test --lib`
- `cargo fmt --check`
- `cargo clippy --lib --tests -- -D warnings`
