# Pipeline-run summary traces with per-job span links

Issue: [#89](https://github.com/PeterGrace/github_webhook_exporter/issues/89)

## Summary

A terminal `workflow_run` delivery now emits a second, independent OTLP trace summarizing one
GitHub Actions run attempt. The root carries one child span per job of that run, and each child
carries an OpenTelemetry span link to the root span of the `github.actions.job` trace that was
exported when that job completed. Operators get a single run waterfall and a one-hop path to the
failing job; per-job traces are unchanged.

## What changed

### New pipeline-run trace

- `src/telemetry/pipeline.rs` holds the bounded pipeline model (`PipelineRunTrace`,
  `PipelineJobSummary`, `WorkflowJobTraceIdentity`) and `WorkflowTraceEmitter::emit_pipeline`.
- Root: span name and `sentry.description` are the sanitized workflow name; `sentry.op` is
  `github.actions.pipeline`; span kind `INTERNAL`.
- Children: `<workflow-name> / <job-name>`, `sentry.op` `github.actions.pipeline.task`, one span
  link each to the corresponding job trace root. Step spans stay on job traces.
- The run conclusion is derived from the summarized jobs with severity descending `failure`,
  `timed_out`, `cancelled`, `other`, `neutral`, `success`, `skipped`. Any failed or timed-out job
  makes the root error (`sentry.status=error`) and sets `error.type`; skipped jobs never mask an
  otherwise successful run.
- The root interval spans the earliest job start to the latest job end; `timing_source` is
  `reported` only when every summarized job used reported timing.
- No `exception` span events and no Sentry errors are raised on the pipeline trace — the job
  traces already report those exactly once.

### New bounded persistence

- Migration `202608190001_create_workflow_job_links.sql` adds `workflow_job_links`, keyed by
  repository, run ID, run attempt, and job ID. It stores only the exported trace/span IDs, the
  sanitized job name, the bounded conclusion, the selected interval, and `updated_at`; the schema
  check constraints make out-of-vocabulary values unrepresentable.
- `WorkflowJobLinkStore` records, lists, and prunes those rows. Listing revalidates every stored
  row against the bounded vocabulary and skips corrupted rows rather than suppressing the whole
  run.
- `WorkflowTraceEmitter::emit` now returns the emitted job root identity so the webhook path can
  persist it; a disabled emitter returns `None` and nothing is written.
- Retention prunes the new table in the same scheduled pass, under the existing processed-delivery
  cutoff, as a fourth bounded workload (`workflow_job_link`).

### Bounds and failure handling

- A run attempt with no recorded job traces emits no pipeline trace. A run attempt with more than
  256 recorded job traces emits no pipeline trace at all, increments
  `github_workflow_job_trace_rejections_total{reason="too_many_jobs"}`, and logs one bounded
  parentless warning. There is never a partial pipeline trace, mirroring the step limit policy.
- Every failure on the pipeline path — link persistence, lookup, or export — degrades to a bounded
  failure metric and leaves the authenticated `204 No Content` response, readiness, and
  merge-queue state untouched.

### Shared code

- `append_pipeline_and_repository_context` and `append_workflow_run_context` were lifted out of
  the job-specific paths so job, step, and pipeline spans build identical shared context from one
  implementation.
- `HistoricalTiming::derived` constructs an interval whose source the caller already decided,
  which the pipeline aggregate and the store's row rehydration both need.

## Open questions resolved

The issue flagged three items for maintainers; they were decided as follows and are documented in
`book/src/reference/traces.md`:

- **Trigger.** `workflow_run` with normalized action `completed`, the only terminal action in the
  existing `Action` vocabulary for that event.
- **Operation names.** `github.actions.pipeline` for the root and
  `github.actions.pipeline.task` for the per-job summary, keeping the root distinguishable from
  the existing `github.actions.job` roots that share a waterfall label.
- **Jobs without emitted traces.** Omitted from the summary. A run whose jobs were all rejected
  (for example by `GHE_WORKFLOW_JOB_MAX_STEPS`) produces no pipeline trace; a partially recorded
  run summarizes only the jobs that were recorded.

## Validation

- `just fmt`
- `cargo clippy --all-targets -- -D warnings`
- `just test`

New coverage: pipeline conclusion precedence and timing aggregation, emitter root/child/link
structure, disabled-emitter no-op, store round-trip with reopen and ordering, run-attempt scoping,
tampered-row skipping, prune batching, redacted persistence failures, run-summary payload
projection, and two end-to-end OTLP tests driving real webhook deliveries through the router.

Not run: live Sentry SaaS verification of how the new `github.actions.pipeline` operation and the
span links render in a Sentry waterfall. The prior `github.actions.job` verification (2026-08-14)
does not cover span links.
