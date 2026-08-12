# OTLP Workflow Exception Events Design

## Objective

Represent failed and timed-out GitHub Actions tasks with vendor-neutral OpenTelemetry exception events while retaining the optional Sentry DSN integration that promotes the same failures into Sentry Issues.

This revises PR #80 without changing its privacy, grouping, or span-linkage requirements.

## Architecture

The historical workflow span is the canonical failure record. Every failed or timed-out step span receives one OpenTelemetry span event named `exception`. A failed or timed-out job span receives one fallback exception event only when no failed or timed-out child step explains the job failure. This preserves the existing duplicate-suppression rule.

The event is exported through the configured OTLP traces endpoint. There is no additional OpenTelemetry error signal or endpoint. Users of any OTLP-compatible backend receive the exception event without configuring Sentry.

When `SENTRY_DSN` is configured, the existing reporter additionally converts the same bounded failure model into a Sentry error envelope. This optional adapter exists only to obtain Sentry-native Issue grouping and linked-error presentation; it is not the canonical representation.

## Exception Event Schema

Each canonical span event has:

- event name: `exception`
- timestamp: the task's selected historical end time
- `exception.type`: `GitHubActionsTaskFailure` or `GitHubActionsTaskTimeout`
- `exception.message`: `CI task failed: <task>` or `CI task timed out: <task>`

`<task>` is the existing sanitized name, limited to 128 Unicode scalar values after control-character removal. When the name is absent, the event uses the existing validated task-run identifier fallback.

The event does not set `exception.stacktrace` because no stack trace exists. It does not set `exception.escaped` because the exporter is describing a historical CI result rather than an exception escaping an instrumented scope.

Existing OTLP span status remains unchanged: failed and timed-out tasks retain `status.code=ERROR` and the fixed status description.

## Data Flow

1. Build the historical job or step span with its exact trace and span IDs.
2. Derive one bounded synthetic workflow failure value from validated workflow data.
3. Add the OpenTelemetry `exception` event to that span before ending it.
4. If a Sentry reporter is configured, use the same value to submit a Sentry error event with matching trace and span IDs.
5. End the span at its historical completion timestamp.

The shared value prevents the OTLP and Sentry representations from drifting in exception type, message, task identity, or conclusion.

## Privacy and Failure Handling

Only already-validated repository, workflow, job, task, conclusion, and task-run identity may enter the shared failure value. The OTLP exception event contains only its fixed type and bounded message. It never contains CI logs, commands, output, payloads, arbitrary GitHub error text, stack traces, or secrets.

OTLP exception events use the existing bounded trace exporter and its established queue/drop behavior. Sentry submission remains optional and non-blocking. Failure of either exporter does not affect webhook responses or readiness.

## Testing

Tests will establish that:

- failed and timed-out step spans export exactly one `exception` event with the expected historical timestamp and semantic attributes;
- non-failing conclusions export no exception event;
- a failed/timed-out job exports one fallback event only when no failed/timed-out child exists;
- the OTLP event is present when no Sentry reporter is configured;
- enabling Sentry does not duplicate the OTLP event;
- the shared bounded value produces matching OTLP and Sentry type/message data;
- serialized OTLP requests contain no prohibited payload fields or raw secret fixtures;
- existing status, trace hierarchy, and Sentry trace/span linkage tests continue to pass.

Validation will include formatting, Clippy with warnings denied, the full test suite, Helm static validation, and Rust documentation generation.
