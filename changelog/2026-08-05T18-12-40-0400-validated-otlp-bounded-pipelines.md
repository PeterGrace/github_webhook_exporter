# Validated OTLP configuration and bounded pipelines

## Summary

Added the optional OTLP/HTTP runtime foundation for Phase 4. Local structured stderr logging remains
active in every mode, while configured trace and log signals export through independently bounded,
non-blocking pipelines.

## Configuration

- Added validated defaults for 2,048 queued records per signal, 512-record batches, and a five-second
  telemetry shutdown timeout.
- Added positive-integer and cross-field validation for `GHE_OTEL_QUEUE_CAPACITY`,
  `GHE_OTEL_BATCH_SIZE`, and `GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS`.
- Added generic and signal-specific OTLP endpoint, header, and timeout resolution with redacted
  failures and debug output.
- Defaulted `service.name` to `github-webhook-exporter`, always attached the package version, and
  allowlisted only Kubernetes pod and namespace resource attributes.
- Kept remote export disabled when no generic or signal-specific endpoint is configured.

## Runtime

- Added minimal OpenTelemetry trace/log, OTLP/HTTP protobuf, and tracing bridge dependencies without
  gRPC transport or an OTLP metrics provider.
- Evolved telemetry initialization into an owned `TelemetryRuntime` with explicit enabled/disabled
  state and provider lifetime ownership.
- Preserved the existing stderr `EnvFilter` and formatter independently of remote layers.
- Added lock-free application admission boundaries around the SDK dedicated-thread processors.
  Exact pending, drop, and failed-export hooks are application-owned, enqueue never waits for
  collector I/O, and SDK batches are programmatically capped.
- Restricted remote application log forwarding to this crate's tracing targets so OpenTelemetry
  exporter diagnostics cannot recurse into OTLP logs.
- Added a force-flush hook for focused pipeline verification and later lifecycle integration;
  final bounded shutdown remains in its dedicated Phase 4 issue.

## Validation

- Added configuration coverage for defaults, overrides, disabled export, malformed/non-Unicode and
  overflowing values, inconsistent capacity/batch settings, endpoint/header validation, service
  defaults, resource allowlisting, and redaction.
- Added exact and concurrent queue-admission tests.
- Added subscriber tests proving stderr remains active in enabled and disabled modes.
- Added an in-process Axum OTLP/HTTP receiver that decodes protobuf requests and proves synthetic
  spans and logs, standard headers, required resources, and configured batch limits.
