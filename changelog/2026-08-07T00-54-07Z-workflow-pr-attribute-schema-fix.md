# 2026-08-07T00:54:07Z - Pull request attribute schema fix

## Summary
- Restored `github.pull_request.number` span attributes to OTLP integer encoding for existing runtime traces.
- Added a dedicated historical workflow-root helper that emits a single bounded OTLP integer array for retained pull request numbers.
- Restored pre-existing OTLP integer assertions outside the new historical root representation.

## Files
- `src/telemetry/trace.rs`
- `src/telemetry/workflow.rs`
- `src/telemetry/otlp_test.rs`

## Verification
- `cargo test telemetry::trace::tests --lib`
- `cargo test telemetry::workflow::tests --lib`
- `cargo test telemetry::otlp_test --lib`
- `cargo fmt --check`
- `cargo clippy --lib --tests -- -D warnings`
