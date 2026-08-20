# Render failed pipeline-run summary spans as errors in Sentry

Closes #92.

## Problem

A failed `github.actions.pipeline.task` span set the OpenTelemetry error status and nothing else.
Sentry ingested `trace.status: error`, but the span itself carried no linked issue and did not
render as an errored span, so a failed run summary looked no different from a successful one in the
waterfall.

## What live Sentry actually does

Verified against Sentry SaaS on 2026-08-19 with three otherwise identical pipeline traces:

| Failure marker on the span                     | `span.status` | Linked issue | Renders as error |
| ---------------------------------------------- | ------------- | ------------ | ---------------- |
| `Status::error("workflow_failed")` only        | `error`       | none         | no               |
| ... plus an `exception` span event             | `error`       | none         | no               |
| ... plus a Sentry error on the span's own IDs  | `error`       | yes          | yes              |

Two findings drove the fix. Sentry's OTLP endpoint does **not** convert OpenTelemetry `exception`
span events into errors, so the span event the issue proposed as the whole fix changes nothing on
its own. And `span.status` was never the missing piece — it was already `error`; what the UI keys
on is an error event whose trace context carries that exact span ID, which is why the job and step
spans always rendered correctly and the pipeline spans never did.

## Change

`WorkflowTraceEmitter::emit_pipeline` now mirrors the job/step path for every job summary whose
conclusion `emits_synthetic_error()`:

- one bounded `exception` span event carrying `exception.type` and `exception.message`, which keeps
  the failure legible to any OTLP backend even though Sentry ignores it; and
- one run-scoped `SyntheticWorkflowError` reported through the configured reporter, whose trace and
  span IDs are the summary span's own.

The root raises nothing. Its conclusion is the most severe job conclusion, so a failing root always
has a failing child that already explains it.

## Keeping the reporting honest

The original design raised nothing here so a failure was reported exactly once. That is no longer
possible — the span cannot render as an error without an error attached to it — so the run-scoped
report is made deliberately distinguishable instead of accidentally duplicative:

- A new `WorkflowTaskKind::PipelineTask` puts `pipeline-task` in the fingerprint where the job and
  step errors carry `job` and `step`, so the run-scoped issue can never merge with them.
- Its description is `CI run job failed: <task>` / `CI run job timed out: <task>`, against the job
  trace's `CI task failed: <task>`, so the two issues are distinguishable in a list.
- Its exception type is unchanged (`GitHubActionsTaskFailure` / `GitHubActionsTaskTimeout`) so it
  still matches the span's own `error.type`.
- Grouping reuses the job-error rules: sanitized job name, or the fixed unnamed-job identity, so
  per-run job IDs stay out of grouping.

`SyntheticWorkflowError::new` now takes an explicit `WorkflowErrorOrigin` instead of borrowing a
`WorkflowJobTrace`, because a pipeline summary has no job trace to borrow from. `for_step` and
`for_job` are unchanged in behavior.

## Verification

- `just fmt`
- `cargo clippy --all-targets -- -D warnings`
- `just test`

New coverage: `pipeline-task` fingerprint separation from job and step errors, the run-scoped
timeout description, unnamed-job grouping stability, span-event presence on failing summaries and
absence on every other conclusion, the root raising nothing, an emitter without a Sentry reporter
still marking the span, and an end-to-end OTLP test driving real webhook deliveries that asserts
the run-scoped Sentry error carries the summary span's own trace and span IDs alongside an
unchanged step-scoped error.

Live Sentry SaaS verification was run against the production emit path (real OTLP exporter, real
`SentryWorkflowErrorReporter`) under marker `gwe92-live-1787177051`. The failed and timed-out
summaries each carry one linked issue (`RUST-C`, `RUST-B`) and `span.status: error`; the successful
summary stays `ok` with no linked issue; the root is `error` with no issue of its own. This closes
the "not run" gap recorded in `2026-08-19T00-00-00Z-pipeline-run-summary-traces.md`.
