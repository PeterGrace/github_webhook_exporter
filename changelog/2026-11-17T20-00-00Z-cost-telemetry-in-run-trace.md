# Show cost telemetry in the workflow run's trace

Date: 2026-11-17T20:00:00Z

## What changed

- `ci cost recorded` logs, for priced jobs and for accepted usage entries, now carry the run's
  derived pipeline trace ID (from repository, run ID and attempt) instead of no trace context. They
  appear in that run's trace, next to the pipeline root. They carry no span ID, because the root's
  span ID is random.
- `ci.usage` spans are exported into the same trace, as roots with no parent span, instead of each
  starting a new random trace.
- The pipeline root's trace ID is unchanged. The derivation moved into a shared
  `run_trace_context` helper that all three use.
- Logs still carry the derived trace ID when trace export is off.
