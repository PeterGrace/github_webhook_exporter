# Bounded workflow telemetry model

- Added the bounded workflow telemetry model for workflow run/job identifiers, sanitized display
  names, normalized conclusions, historical timing, and owned workflow job/step traces.
- Centralized workflow OTLP attribute keys and pure KeyValue builders for semconv and diagnostic
  identifiers.
- Added focused policy tests for identifier validation, name sanitization, conclusion normalization,
  status mapping, and step-number rejection.
