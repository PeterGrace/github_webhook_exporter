# Bounded telemetry provider shutdown

- Added one idempotent `TelemetryRuntime` shutdown operation that closes admission and starts trace
  and log provider shutdown concurrently under one shared deadline.
- Preserved exact queue occupancy accounting: exported records release pending slots, deadline-
  stranded slots are atomically converted to `pipeline_closed` drops, and records produced after
  closure increment the same bounded counter.
- Routed post-telemetry startup failures, SIGINT, SIGTERM, server completion/errors, and HTTP drain
  timeouts through the same final telemetry cleanup path without replacing the service result.
- Added controlled concurrency, timeout, normalized failure, process lifecycle, OTLP export-at-exit,
  and complete integrated privacy regression coverage; suppression concurrency assertions now
  accept every valid split while proving the exact total.
- Documented disabled mode, endpoint/header examples, queue and batch bounds, identifier policy,
  diagnostics, counters, and HTTP/retention-before-telemetry shutdown ordering.
