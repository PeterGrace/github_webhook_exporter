# OTLP failure and dropped-record diagnostics

## Changes

- Added complete bounded Prometheus families for trace/log export failures and rejected records.
- Shared one metrics registry between telemetry pipeline hooks and the HTTP `/metrics` endpoint.
- Added exact lock-free `queue_full` and `pipeline_closed` observation at the application admission
  boundary.
- Added a direct stderr observer with independent atomic one-minute limiters per signal/reason and
  suppression accounting; diagnostics bypass `tracing` and OpenTelemetry logs.
- Wrapped the OTLP HTTP client to classify timeout, transport, non-success response, and malformed
  protobuf failures before the SDK erases structured details.
- Preserved the OpenTelemetry blocking-client construction thread boundary so async startup does not
  nest or drop Reqwest's internal runtime inside Tokio.
- Kept endpoint, header, response-body, error-text, payload, and identifier values out of diagnostic
  labels and output.

## Validation coverage

- Exact pre-seeded metrics and shared registry ownership.
- Controlled-time and concurrent rate limiting with exact suppression totals.
- Queue capacity and closure drops.
- Structured Reqwest classification and malformed OTLP response classification.
- Existing blocked-collector, queue saturation, webhook `204`, readiness, OTLP protobuf, and privacy
  regression suites.
