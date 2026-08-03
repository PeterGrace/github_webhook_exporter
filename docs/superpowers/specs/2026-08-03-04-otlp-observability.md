# Specification 4: OTLP Observability

## Goal

Export bounded, privacy-preserving traces and logs over OTLP/HTTP without making webhook processing
or readiness depend on collector availability.

## Dependencies

Specifications 1 and 2 must be complete. Specification 3 is optional; when present, its queue spans
and events are enabled.

## Configuration

Honor standard OpenTelemetry variables supported by the selected Rust SDK, including:

- `OTEL_EXPORTER_OTLP_ENDPOINT`
- `OTEL_EXPORTER_OTLP_HEADERS`
- `OTEL_EXPORTER_OTLP_TIMEOUT`
- `OTEL_SERVICE_NAME`, defaulting to `github-webhook-exporter`
- `OTEL_RESOURCE_ATTRIBUTES`

Application defaults are a 2,048-record queue per signal, batches of at most 512 records, and a
five-second shared shutdown timeout. `GHE_OTEL_QUEUE_CAPACITY`, `GHE_OTEL_BATCH_SIZE`, and
`GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS` accept validated positive integer overrides; batch size must not
exceed queue capacity. Invalid telemetry configuration fails startup rather than silently disabling
requested export. Absence of an OTLP endpoint leaves local structured stderr logging active and
disables remote export.

## Telemetry pipeline

- Use OTLP/HTTP for traces and logs.
- Use bounded batch processors; never block request handling indefinitely.
- Preserve structured stderr logs independently of the OTLP pipeline.
- Collector connection failures, queue saturation, and dropped records do not affect readiness or
  webhook responses.
- Rate-limit local exporter failure reports to prevent a collector outage from flooding stderr.
- Flush trace and log providers during graceful shutdown within a shared fixed timeout.

## Spans

Primary spans are:

```text
http.request
github.webhook.authenticate
github.webhook.process
config.repository.write
sqlite.query
merge_queue.update
```

`merge_queue.update` exists only when specification 3 is installed. Nested spans follow the request
lifecycle; internal maintenance tasks create independent roots.

The attribute allowlist contains HTTP method, route template, response status, normalized result,
normalized event type/action, database operation name, and normalized queue outcome. Resource
attributes contain service name and version plus pod name and Kubernetes namespace when those two
values are explicitly configured.

Forbidden attributes include payloads, request bodies, repository names, repository IDs,
pull-request numbers, SHAs, delivery IDs, signatures, secrets, authorization headers, raw URLs,
raw event actions, and raw dequeue reasons. Delivery IDs are excluded entirely rather than treated
as trace context.

## Failure visibility

Maintain local Prometheus counters for normalized telemetry failures and dropped records. SDK/export
errors flow to stderr without recursively entering the OTLP log pipeline, limited to one report per
normalized error category per minute. No telemetry failure changes an authenticated webhook's `204`
response.

## Tests

- An in-process mock OTLP/HTTP receiver captures trace and log exports.
- Captured telemetry contains required span structure and permitted attributes.
- Captured telemetry and stderr are scanned for every forbidden value class.
- Collector outage and slow-response tests prove request latency remains bounded and readiness stays
  healthy.
- Queue-saturation tests prove drops are counted and locally reported without recursion.
- Shutdown tests prove both providers receive a flush attempt and respect the shared timeout.
- Configuration tests cover defaults, overrides, disabled export, and invalid values.

## Acceptance criteria

- Traces and logs export over OTLP/HTTP when configured.
- Collector unavailability never changes readiness or webhook acceptance.
- Export queues and shutdown waits are bounded.
- Dropped telemetry is visible through local logs and Prometheus counters.
- No forbidden sensitive or unbounded values appear in spans, logs, or resources.
