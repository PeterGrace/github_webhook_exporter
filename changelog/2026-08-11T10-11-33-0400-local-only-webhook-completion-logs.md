# Local-only webhook completion logs

Changed the generic `GitHub webhook request processed` event from INFO to DEBUG and assigned it an
explicit local-only tracing target. Operators can enable the event through `RUST_LOG`, while the
OpenTelemetry log layer now rejects that target without suppressing other application DEBUG logs.

Added unit coverage for the target admission policy and an end-to-end webhook regression proving
the event remains visible locally but absent from exported OTLP protobuf logs.

## Validation

- `cargo test telemetry::otlp_test::webhook_completion_is_local_debug_only --lib -- --exact`
- `cargo test --all-targets`
- `cargo build --all-targets`
- `cargo clippy --all-targets -- -D warnings`
- `cargo fmt --all -- --check`
