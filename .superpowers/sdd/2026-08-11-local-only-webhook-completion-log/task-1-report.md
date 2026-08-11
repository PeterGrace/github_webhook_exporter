# Task 1 Report

## Implementation details
- Added `pub(crate) const LOCAL_ONLY_LOG_TARGET` for the local diagnostics target.
- Added private `is_remote_log_target(target: &str) -> bool` and log-specific `application_log_metadata`.
- Switched the OTLP log bridge to filter with `application_log_metadata`.
- Kept `is_application_target` and the trace filter behavior unchanged.
- Added a focused regression test for the local-only target admission policy.

## Files changed
- `src/telemetry.rs`
- `changelog/2026-08-11T14-33-49Z-task-1-local-only-otlp-log-admission-policy.md`

## RED / GREEN
- RED command: `cargo test telemetry::tests::remote_log_filter_rejects_only_the_local_only_application_target --lib -- --exact`
- RED output: `error[E0432]: unresolved imports super::is_remote_log_target, super::LOCAL_ONLY_LOG_TARGET`
- Why RED was expected: the test imported names that did not exist yet, proving the policy surface was still missing.
- GREEN command: `cargo test telemetry::tests::remote_log_filter_rejects_only_the_local_only_application_target --lib -- --exact`
- GREEN output: `test telemetry::tests::remote_log_filter_rejects_only_the_local_only_application_target ... ok`
- GREEN command: `cargo test telemetry::tests::remote_layers_accept_only_application_targets --lib -- --exact`
- GREEN output: `test telemetry::tests::remote_layers_accept_only_application_targets ... ok`

## Self-review
- The new admission path is narrow and only affects the OTLP log bridge.
- Trace admission still uses the existing application namespace predicate.
- The regression test covers both the local-only target and ordinary application targets.

## Concerns
- None.
