# Fix Round 1: Central Telemetry Policy

## Summary

Centralized HTTP and repository configuration trace policy in `src/telemetry/trace.rs`.

## Changes

- Moved HTTP attribute keys, method mapping, response-result mapping, and setters into `trace.rs`.
- Added typed helpers: `set_http_method`, `set_http_route`, `set_http_response`, and `set_config_operation`.
- Added bounded `HttpMethod`, `HttpResult`, and `ConfigOperation` enums with focused variant coverage.
- Restricted route recording to `Option<&axum::extract::MatchedPath>` so callers cannot pass raw URI paths, query strings, or arbitrary identifiers; `None` records only the literal `unmatched`.
- Updated repository write instrumentation to use `ConfigOperation` and centralized `set_result_status`.
- Added a typed `RepositoryMetadata::canonical_full_name` accessor so update spans do not need raw-string repository-name setters.

## Validation

- `cargo test telemetry::otlp_test::repository --lib && cargo test --test repository_api`
- `cargo fmt --check`
- `cargo clippy -- -D warnings`
- `cargo test`
- `cargo build`
