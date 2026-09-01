# Job span `sentry.description` narrowed to the bare job name

Trims the `sentry.description` attribute on `github.actions.job` root spans (issue #97).

- A job span's `sentry.description` is now the bare `<job-name>` instead of the compound
  `<workflow-name> / <job-name>`. The workflow name is already carried on the same span as
  `cicd.pipeline.name`, so repeating it inside the description only added noise to Sentry's
  description column.
- A new private `job_description` helper in `src/telemetry/workflow.rs` mirrors the existing
  `step_description`, keeping the job and step description rules symmetric and documented in one
  place each.
- The job span's **name** is unchanged: it stays the compound `<workflow-name> / <job-name>` built
  by `job_span_name`, so waterfall rows still carry the full path.
- The synthetic Sentry error `trace_description` for a failed or timed-out job
  (`src/telemetry/workflow_error.rs::for_job`) is deliberately left on `job_span_name`. Narrowing
  it was explicitly out of scope for issue #97; if the reporter wants the error description to
  follow the span description the way step errors do, that is a separate change.
- Step span descriptions, `sentry.op` values, and every other job attribute are untouched.

When a job has no reported name, the description is the fixed `UNNAMED_JOB_NAME` fallback `"job"`.
`book/src/reference/traces.md` is updated to match.
