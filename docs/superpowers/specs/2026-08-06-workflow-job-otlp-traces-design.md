# Completed Workflow Job OTLP Trace Design

## Purpose

Export each newly claimed `workflow_job.completed` webhook as one self-contained historical
OpenTelemetry trace. The trace represents the completed GitHub Actions job as its root and every
reported step as a direct child while preserving the exporter's existing response, deduplication,
privacy, boundedness, persistence, and non-blocking guarantees.

## Scope

The feature processes only authenticated `workflow_job` events whose normalized action is
`completed`, and only after the durable delivery claim returns `New`. Queued and in-progress
actions, duplicate delivery identifiers, and malformed or unsupported workflow-job projections do
not emit workflow spans. Generic webhook metrics and responses remain unchanged.

Each accepted projection emits:

- one independent root span named `github.workflow.job`;
- one direct child span named `github.workflow.step` for every reported step; and
- no durable workflow payload, identifier, or cross-delivery correlation state.

A workflow-run root spanning multiple jobs remains out of scope.

## Architecture

### Authenticated projection

A focused `src/api/workflow_job.rs` module owns the minimal workflow-job payload projection and all
normalization required to construct safe historical spans. The existing webhook handler continues
to authenticate, normalize the generic event/action, and claim the delivery. After a new claim, it
passes completed workflow-job payloads to this module. A second parse at this boundary is
intentional: specialized workflow fields are not interpreted before authentication and
at-most-once admission.

The projection accepts only the fields needed for telemetry:

- workflow display name;
- workflow run ID and attempt;
- job ID, display name, conclusion, head SHA, start time, and completion time;
- pull-request numbers; and
- each step's number, display name, conclusion, start time, and completion time.

Required identifiers must be positive integers. The head SHA must pass the shared `CommitSha`
validator. At most the first 20 positive pull-request numbers are retained. Fields such as commands,
output, logs, actors, URLs, and arbitrary payload fragments are not represented by the projection.

### Historical trace emitter

A focused emitter in the telemetry layer uses an `SdkTracer` cloned from the already configured
trace provider. It creates spans through OpenTelemetry `SpanBuilder` so start and end timestamps can
be supplied explicitly. The root is built against an empty OpenTelemetry context, making it
independent of the live webhook request trace. Child steps are built with the job root context.

The emitter remains optional. When trace export is disabled, processing is a no-op. When enabled,
finished historical spans enter the same bounded, non-blocking span processor and queue used by
ordinary application spans. Collector failures therefore do not alter webhook responses or block
request handling.

`TelemetryRuntime` exposes a cloneable emitter handle. `AppState` stores the handle, defaults to a
disabled emitter for existing constructors and tests, and provides an explicit builder-style method
used by production startup and OTLP integration fixtures.

## Attribute policy

All operation names and attribute keys are constants owned by the telemetry policy. Untrusted text
is never used as a span name or log field. Prometheus uses the authenticated canonical repository
full name and otherwise accepts only bounded normalized label values.

Where the OpenTelemetry CI/CD registry has a compatible attribute, spans use:

- `cicd.pipeline.name` for the sanitized workflow name;
- `cicd.pipeline.run.id` for the workflow run ID encoded as a decimal string;
- `cicd.pipeline.task.name` for the sanitized job or step name;
- `cicd.pipeline.task.run.id` for the job or step run identifier encoded as a decimal string;
- `cicd.pipeline.result` for compatible job results; and
- `cicd.pipeline.task.run.result` for compatible job and step results.

The semantic-convention result vocabulary is `success`, `failure`, `timeout`, `cancellation`, and
`skip`. GitHub conclusions are independently normalized to `success`, `failure`, `cancelled`,
`skipped`, `timed_out`, `neutral`, or `other`. The exact normalized GitHub value is recorded in a
bounded GitHub-namespaced conclusion attribute. A semantic-convention result is omitted for
`neutral` and `other` rather than emitting an invalid convention value.

Validated diagnostic identifiers remain span-only attributes:

- canonical repository name;
- delivery UUID;
- workflow run ID;
- workflow run attempt;
- workflow job ID;
- validated head SHA; and
- no more than 20 positive pull-request numbers.

Display names are also span-only. Sanitization removes every Unicode control character and retains
at most the first 128 remaining Unicode scalar values. Empty sanitized names are omitted.

## Timing

GitHub timestamps are parsed as RFC 3339 values and converted to `SystemTime` without accepting
unbounded text into telemetry.

A job uses reported timing only when both timestamps parse and `started_at <= completed_at`. Its
root receives those exact historical boundaries and `timing_source=reported`. Otherwise the job is
instantaneous at the parsed job completion timestamp, or at request receipt when completion is
missing or malformed, and receives `timing_source=fallback`.

A step uses reported timing only when both timestamps parse, are ordered, and fall within the job's
selected parent interval. Otherwise it is instantaneous at the job's selected end timestamp and
receives `timing_source=fallback`. This rule includes missing, malformed, reversed, and
out-of-parent timestamps while ensuring every reported step is represented.

## Status

Job and step conclusions control OpenTelemetry span status:

- `success` sets `Status::Ok`;
- `failure` and `timed_out` set an error status with a fixed bounded description;
- `cancelled`, `skipped`, `neutral`, and `other` leave status unset.

Raw unrecognized conclusions are never exported.

## Error handling and lifecycle

Malformed or unsupported specialized projections are ignored after generic webhook accounting.
They do not change the authenticated `204 No Content` response. Historical export is best-effort:
queue drops and collector failures are accounted for by the existing telemetry runtime and do not
propagate into webhook handling.

The implementation does not spawn per-delivery tasks, wait for batching, retain request payloads,
or introduce locks in the request path beyond those already present in the bounded exporter.

## Privacy guarantees

The workflow projection and emitted spans never export or log:

- commands, output, or logs;
- actors or user data;
- raw or derived URLs;
- request bodies or payload fragments;
- webhook signatures, repository secrets, authorization headers, or OTLP headers; or
- raw unsupported actions or conclusions.

Approved workflow identifiers and sanitized names appear only in OTLP spans, except that the
canonical authenticated repository name also labels repository-scoped Prometheus metrics.
Integration tests scan captured spans, OTLP logs, structured stderr, and Prometheus exposition to
enforce the boundary.

## Validation

Unit tests cover:

- all supported and unknown conclusion mappings and statuses;
- Unicode-control removal and the 128-character name limit;
- positive identifier validation and the 20-PR cap;
- reported and fallback job timing; and
- reported, missing, malformed, reversed, and out-of-parent step timing.

In-process OTLP receiver tests cover:

- one independent job root and one correctly parented child per reported step;
- explicit historical timestamps and timing-source attributes;
- success, failure, cancellation, skip, timeout, neutral, and unknown conclusions;
- unsupported actions and malformed projections;
- duplicate delivery suppression;
- collector unavailability without response or state changes; and
- span-only identifier/name visibility, authenticated repository metric labeling, and
  forbidden-value absence across spans, logs, stderr, and Prometheus exposition.

The final verification sequence is:

1. `just fmt`
2. `cargo build`
3. `cargo clippy --all-targets -- -D warnings`
4. `just test`
5. `cargo doc --no-deps`
