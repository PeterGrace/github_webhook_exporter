# Task 2: HTTP Roots and Repository API Hierarchy

## Summary

Implemented bounded OpenTelemetry spans for HTTP request roots and repository configuration write children.

## Changes

- Added an all-router Axum middleware that creates `http.request` spans.
- Recorded bounded HTTP method, route template, response status code, and result class attributes.
- Preserved privacy by reading only `MatchedPath` route templates instead of raw URIs or query strings.
- Added `config.repository.write` child spans for repository create, update, and delete store futures.
- Recorded bounded repository write operations and success/failure outcomes without error text.
- Enabled Axum's `matched-path` feature so route templates are available to middleware.
- Added OTLP hierarchy tests covering create, list, update, get, delete, malformed JSON, unauthorized, unknown route, and store failure cases.

## Verification

- `cargo test telemetry::otlp_test::repository --lib -- --nocapture`
- `cargo test telemetry::otlp_test::repository --lib && cargo test --test repository_api`
- `cargo test`
- `cargo fmt --check`
- `cargo clippy -- -D warnings`
- `cargo build`
