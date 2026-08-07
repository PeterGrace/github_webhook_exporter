# Workflow Job Step Limit Design

## Purpose

Bound the CPU, memory, export, and downstream rendering cost of completed workflow-job traces while
preserving complete traces for realistic GitHub Actions jobs. The service will enforce a
configurable semantic step limit, measure observed job sizes, and make every limit rejection
traceable to the affected GitHub job.

This specification amends the completed workflow-job trace design. Requirements in
`2026-08-06-workflow-job-otlp-traces-design.md` remain in force except that “every reported step”
applies only when the reported step count is within the configured limit.

## Configuration

Add the positive integer environment variable `GHE_WORKFLOW_JOB_MAX_STEPS` with these constraints:

- default: `256`;
- minimum: `1`;
- hard maximum: `1024`; and
- no zero or unlimited mode.

A missing variable selects the default. A non-Unicode, non-numeric, zero, overflowing, or
above-maximum value fails startup with a redacted configuration error that names only
`GHE_WORKFLOW_JOB_MAX_STEPS`.

`RuntimeConfig` owns the validated value and exposes it as a read-only `usize`. Production startup
passes the value explicitly into `AppState`; the webhook handler reads the immutable value from
application state.

## Bounded admission

The specialized workflow-job path remains restricted to authenticated, newly claimed
`workflow_job.completed` deliveries. Duplicate and unsupported deliveries do not run workflow-job
admission and do not update the new workflow metrics.

Before constructing the detailed workflow model, the handler performs a minimal admission parse.
The admission projection retains only validated positive workflow run ID, run attempt, and job ID,
and counts the elements in the `steps` array. A custom Serde sequence visitor consumes each step as
`IgnoredAny`, increments a checked counter, and immediately discards the element. It never stores a
step collection, display name, timestamp, conclusion, command, output, log, actor, URL, or arbitrary
payload fragment.

Missing `steps` is treated as an empty array for compatibility with the existing projection. A
non-array `steps`, invalid required identifier, malformed envelope, or counter overflow rejects the
specialized projection without workflow metrics, warning, or historical spans. The generic webhook
response and metrics retain their existing behavior.

For an admitted envelope, the exact reported step count is observed before applying the configured
limit:

- `step_count <= step_limit`: run the existing detailed projection and emit one complete job trace;
- `step_count > step_limit`: record the bounded rejection telemetry and emit no historical job or
  step span.

The accepted path intentionally parses the small step array a second time to build the detailed
model. This avoids complex runtime-seeded deserialization while ensuring an oversized delivery is
never materialized as an unbounded step vector. The immutable request body guarantees both passes
see the same input.

The durable delivery claim and authenticated `204 No Content` response are unchanged. A rejected
delivery remains claimed and is not retried, because changing the limit later must not create
duplicate generic accounting or surprise historical emission.

## Prometheus metrics

Register the unlabeled histogram `github_workflow_job_steps` with finite buckets:

```text
0, 5, 10, 20, 40, 64, 128, 256, 512, 1024
```

Observe one value for every newly claimed completed workflow job whose admission envelope,
identifiers, and step array are structurally valid. Accepted and over-limit jobs are both observed.
Malformed projections and duplicates are not observed. Repository names and workflow identifiers
are never metric labels.

Register the counter family:

```text
github_workflow_job_trace_rejections_total{reason="too_many_steps"}
```

The rejection reason is represented by a closed Rust enum. Seed the single supported reason at
startup so the family is visible with value zero before the first rejection. Increment it exactly
once for each admitted job whose step count exceeds the configured limit.

## Actionable rejection diagnostic

Every `too_many_steps` rejection emits one warning through the normal structured logging pipeline.
Structured stderr is always active; when OTLP log export is enabled, the same warning may also be
exported through the existing bounded log queue.

The warning has a fixed message and these fields:

- `reason="too_many_steps"`;
- canonical `repository_name`;
- positive `workflow_run_id`;
- positive `workflow_run_attempt`;
- positive `workflow_job_id`;
- authenticated `delivery_id`;
- exact `step_count`; and
- configured `step_limit`.

These identifiers deliberately amend the previous spans-only privacy rule for this one bounded
warning. They let an operator locate the job with GitHub’s repository-scoped workflow-job API and
correlate it with the webhook delivery. Workflow, job, and step names; commit SHA; pull-request
numbers; actors; commands; output; logs; raw or derived URLs; payload fragments; signatures;
secrets; authorization headers; and collector details remain forbidden.

A Prometheus alert should use the rejection counter, while the structured warning supplies the
high-cardinality identifiers needed for investigation. No identifier is added to Prometheus.

## Historical trace behavior

An accepted job continues to emit exactly one independent `github.workflow.job` root and one direct
`github.workflow.step` child for every reported step, preserving order, historical timing,
conclusion normalization, attributes, privacy constraints, and the bounded non-blocking exporter.
The configured limit is inclusive.

An over-limit job emits neither the root nor any child. The service never truncates a workflow
trace, because a partial tree would falsely appear complete and could omit the failing or otherwise
important step.

## Validation

Test-driven implementation will cover:

- the default value of 256 and valid overrides at 1 and 1024;
- rejection of non-Unicode, malformed, zero, overflowing, and above-1024 configuration values;
- explicit propagation of the validated limit into application state;
- zero-step and missing-step-array admission;
- exact acceptance at the configured limit and complete ordered projection;
- whole-trace rejection at limit plus one;
- bounded counting of a large step array without retaining its elements;
- histogram observations for accepted and over-limit jobs;
- one `too_many_steps` counter increment per rejected newly claimed delivery;
- all required warning identifiers and numeric fields;
- absence of forbidden payload values from metrics and the warning;
- no workflow spans for an over-limit job;
- duplicate delivery suppression for traces, workflow metrics, and rejection warnings; and
- an unchanged authenticated `204 No Content` response.

Documentation will describe the environment variable, hard maximum, histogram buckets, rejection
counter, all-or-nothing trace behavior, identifier exception, and GitHub job lookup procedure. A
timestamped changelog entry will record the amendment.

The final verification sequence is:

1. `just fmt`
2. `cargo build`
3. `cargo clippy --all-targets -- -D warnings`
4. `just test`
5. `cargo doc --no-deps`
