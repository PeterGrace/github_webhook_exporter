# Workflow Job Trace Design

- Recorded the approved design for exporting completed GitHub Actions jobs and steps as
  independent historical OTLP traces.
- Defined authenticated projection, timestamp fallback, conclusion normalization, span-only
  identifier, privacy, deduplication, and non-blocking export boundaries.
- Selected a dedicated historical emitter backed by the existing bounded OpenTelemetry trace
  provider.
