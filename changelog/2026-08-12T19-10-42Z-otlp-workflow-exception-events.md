# 2026-08-12T19:10:42Z OTLP workflow exception events task 2

## Summary
- Documented canonical OpenTelemetry `exception` span events as the baseline representation for failed and timed-out workflow tasks.
- Clarified that `SENTRY_DSN` optionally adds Sentry-native error promotion and does not disable OTLP export.
- Updated trace, telemetry, and remote-telemetry how-to docs to state that no Sentry configuration is required for the canonical representation.
