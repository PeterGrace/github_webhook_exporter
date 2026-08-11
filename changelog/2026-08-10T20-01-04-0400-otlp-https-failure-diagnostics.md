# OTLP HTTPS and Failure Diagnostics

## Changed

- Enabled rustls on the existing blocking reqwest OTLP client so HTTPS trace and log endpoints can
  establish TLS without OpenSSL in the runtime image.
- Added bounded HTTP status and transport-detail fields to direct exporter failure diagnostics.
- Changed exporter failures to emit one direct stderr line per failed attempt while retaining
  rate-limited queue-drop diagnostics.
- Made exporter-failure tests deterministic under concurrent CI execution by using a bounded
  closed-port fixture and accepting the deliberate HTTP 503 flush failure.

## Security

Diagnostics continue to exclude endpoint URLs, headers, credentials, request payloads, collector
response bodies, and raw dependency errors. They bypass tracing and the OTLP log pipeline.

## Validation

- `cargo test`
- `cargo build`
- `cargo clippy --all-targets -- -D warnings`
- `cargo fmt --check`
- `cargo doc --no-deps`
