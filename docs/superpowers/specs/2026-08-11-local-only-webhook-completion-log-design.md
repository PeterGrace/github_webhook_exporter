# Local-Only Webhook Completion Log Design

## Purpose

Reduce routine telemetry volume by making every `GitHub webhook request processed` event a local
DEBUG diagnostic that is never exported through the OpenTelemetry log pipeline.

## Scope

The policy applies to the generic completion event emitted after every GitHub webhook response,
regardless of response status. It does not change webhook responses, request metrics, traces,
specialized warning or error events, or the export policy for any other DEBUG event.

## Design

The webhook observation middleware will emit `GitHub webhook request processed` at DEBUG instead of
INFO and assign it a dedicated local-only tracing target. The local formatting layer will continue
to process the event according to `RUST_LOG`, allowing operators to enable it when troubleshooting.

The OpenTelemetry log layer will use a log-specific metadata filter that rejects the dedicated
local-only target before records reach the OTLP queue. Existing application-target filtering will
remain in effect for all other logs. Trace filtering remains unchanged.

A dedicated target is preferable to a global OTLP INFO threshold because it avoids suppressing
other DEBUG records that an operator intentionally enables. It is also more explicit and stable
than filtering by message text, source location, or an incidental event field.

## Testing

Regression coverage will configure DEBUG local logging and an enabled OTLP log exporter, process a
webhook, flush telemetry, and prove both sides of the contract:

- local structured output contains `GitHub webhook request processed` at DEBUG; and
- captured OTLP logs do not contain that event.

Focused metadata-filter tests will verify that the local-only target is rejected while ordinary
application targets remain eligible. Existing webhook metrics, tracing, and error-path tests will
continue to validate unchanged behavior.

## Documentation

A timestamped entry under `changelog/` will record the new local-only completion-log policy and the
validation performed.
