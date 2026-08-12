# Task 3 clippy validation fix

## Summary
- investigated the `cargo clippy --all-targets -- -D warnings` failure on `feat-issue-78-linked-sentry-errors`
- confirmed the failure was branch-caused by the new explicit tuple annotation in `src/telemetry/otlp_test.rs`
- factored the long tuple type into a local `WorkflowConclusionCase` alias to satisfy `clippy::type-complexity` without changing test behavior

## Validation target
- `cargo clippy --all-targets -- -D warnings`

## Files changed
- `src/telemetry/otlp_test.rs`
