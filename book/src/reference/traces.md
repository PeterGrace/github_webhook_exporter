# Traces

Exported over OTLP when [remote telemetry](telemetry.md) is enabled. Identifiers below are
span-only unless stated otherwise. Workflow failures and timeouts export bounded OpenTelemetry
`exception` span events as the canonical representation; `SENTRY_DSN` optionally promotes the same
failures to synthetic Sentry workflow errors. There is no separate OTLP errors endpoint. The one
bounded workflow-rejection warning noted below also remains; neither path permits raw payload data,
logs, commands, output, or secrets.

## Core service operations

Six stable operations:

- `http.request` — one root per HTTP request; authenticated webhook roots include
  `github.repository.name`.
- `github.webhook.authenticate` — repository lookup and HMAC authentication, under the request
  root.
- `github.webhook.process` — payload projection, durable delivery claim, and bounded event
  processing, under an authenticated request.
- `config.repository.write` — create, update, or delete persistence, under an admin API request.
- `sqlite.query` — one child per logical repository, delivery, or merge-queue store operation.
- `merge_queue.update` — one specialized merge-group or pull-request transition, under webhook
  processing.

Each scheduled delivery/merge-queue pruning pass emits an independent `retention.run` root, with
`delivery.prune` and `merge_queue.prune` as children — independent even when the retention task
starts from a context carrying another span.

`http.route` carries the Axum route template (for example `/api/v1/repositories/{id}`); unmatched
requests use the fixed value `unmatched`. Raw paths, query strings, and URLs are never trace
attributes. HTTP methods, response classes, repository operations, webhook event/action values,
database operations, merge-group reasons, queue outcomes/reasons, terminal outcomes, and failure
events all use closed, bounded vocabularies.

Repository names, database identifiers, delivery identifiers, pull-request numbers, valid full
commit SHAs, workflow identifiers, and sanitized workflow names are diagnostic span attributes.
Canonical authenticated repository names also label repository-scoped Prometheus metrics — see
[Metrics](metrics.md).

Duplicate delivery claims emit an authentication span and a process span with outcome
`duplicate`, and no second `merge_queue.update` span. Queue-state persistence failures keep the
authenticated `204 No Content` response and emit only error status plus an `operation.failure`
event with bounded reason `queue_state`. A merge-group `destroyed` event with reason `dequeued`
records normalized group reason `dequeued`; a pull-request dequeue records outcome `unknown` and
reason `unclassified_dequeue`. Retention roots report only `success`, `cancelled`, or `failure`,
with no cutoff or row identifiers.

Trace attributes and events never include request bodies or payload fragments, repository secrets,
webhook signatures, authorization or OTLP headers, actors, raw URLs, commands, raw actions or
reasons, SQL statements, database paths, or internal error text.

## Completed workflow traces

Only an authenticated `workflow_job` webhook whose normalized action is `completed` can emit a
historical workflow trace. Admission happens only after the durable delivery claim returns `New` —
a duplicate, unsupported action, or malformed specialized projection emits no workflow spans. A
zero or negative step number rejects the entire projection; there is no partial root-and-children
trace. Admission is at most once per delivery, including across a process interruption after the
claim commits.

`GHE_WORKFLOW_JOB_MAX_STEPS` (default `256`, range `1..=1024`, no unlimited mode) bounds admission,
applied after the delivery claim. A newly claimed over-limit job stays durably claimed and still
returns authenticated `204 No Content`.

Every structurally valid newly claimed completed job updates `github_workflow_job_steps{repository}`
once with its reported step count, regardless of later acceptance or rejection. Histogram buckets
are `0`, `5`, `10`, `20`, `40`, `64`, `128`, `256`, `512`, `1024`, plus `+Inf`. Accepted jobs emit
every reported step as a span; over-limit jobs emit no partial trace and increment
`github_workflow_job_trace_rejections_total{reason="too_many_steps"}` once.

Each accepted projection creates an independent `github.workflow.job` root, with a trace identity
unrelated to the live `http.request` trace. Every projected step is a direct `github.workflow.step`
child; payload-provided names never become span names. The service creates no workflow-run root and does not mutate merge-queue state for a
`workflow_job` event. It persists only bounded workflow-run correlation metadata keyed by
repository, run ID, and run attempt; these records use the processed-delivery retention cutoff.

**Timing.** A job uses its exact RFC 3339 `started_at`/`completed_at` only when both parse and
start is not after completion (`timing_source=reported`); otherwise it's instantaneous at a valid
completion time, or at request-receipt time when completion is missing or malformed
(`timing_source=fallback`). A step uses reported timing only when both values parse, are ordered,
and lie inside the selected job interval; every other step is instantaneous at the job end and
marked `fallback`.

**Conclusions.** Normalized to `success`, `failure`, `cancelled`, `skipped`, `timed_out`,
`neutral`, or `other`. The CI/CD result is respectively `success`, `failure`, `cancellation`,
`skip`, or `timeout` where that semantic exists, omitted for `neutral`/`other`. `success` sets
OpenTelemetry status OK; `failure` and `timed_out` set error status with a fixed description; all
other conclusions leave status unset. Raw unknown conclusions are discarded.

Every failed or timed-out step emits one bounded OpenTelemetry `exception` span event. When
`SENTRY_DSN` is configured, the same historical step also emits one synthetic Sentry error whose
trace and span IDs match that step. A failed/timed-out job emits a job-level fallback only when no
failed/timed-out child explains it. Exception types are fixed (`GitHubActionsTaskFailure` or
`GitHubActionsTaskTimeout`); the description includes the sanitized task name, or the validated
task-run ID when a name is absent. Fingerprints combine the bounded repository, workflow, job,
task, and conclusion values so repeated failures group by CI task rather than workflow run. These
Sentry events are synthetic and handled, contain no stack trace, logs, commands, or output, and use
Sentry's bounded non-blocking transport.

**Identifiers and run context.** The workflow root carries only these validated span-only
identifiers: canonical repository name, delivery UUID, workflow run ID, positive run attempt,
workflow job ID, valid full head SHA, and at most the first 20 positive pull-request numbers. Run ID
is exported as both
`cicd.pipeline.run.id` and `github.workflow.run.id`. Job ID is exported as a decimal string under
both `github.workflow.job.id` and the root's `cicd.pipeline.task.run.id`; each step's
`cicd.pipeline.task.run.id` is `<job-id>:<positive-step-number>`. Workflow, job, and step display
names are span-only, sanitized by removing all Unicode control characters and keeping at most the
first 128 remaining Unicode scalar values; an empty result is omitted.

An earlier authenticated `workflow_run` delivery supplies the authoritative normalized
`github.workflow.event` and sanitized `github.workflow.source_branch` and
`github.workflow.target_branch`. Correlation uses repository, run ID, and run attempt. The event is
`pull_request`, `merge_group`, `push`, or `other`; branches remove control characters and retain at
most 255 Unicode scalar values. Missing or ambiguous branches are omitted. The same available
context is attached to the job root and every step.

The job root also carries `github.workflow.job.url`, derived only from the validated repository,
run ID, and job ID. Each step carries `github.workflow.step.url`, which appends GitHub's
`#step:<step-number>:1` log anchor. Payload-provided URLs remain ignored. Derived URLs are span-only
and never enter logs or metrics.

Commands, output, logs, actors, payload-provided URLs, request bodies, arbitrary payload fragments,
secrets, signatures, authorization/other headers, unsupported actions, and raw unknown conclusions
never enter workflow telemetry, in traces, logs, or metrics. The one exception is a parentless
structured warning on a newly claimed `too_many_steps` rejection, which may contain only
`repository_name`, `delivery_id`, `workflow_run_id`, `workflow_run_attempt`, `workflow_job_id`,
`step_count`, and `step_limit`. Operators can query
`GET /repos/{owner}/{repo}/actions/jobs/{job_id}` with the canonical repository name and
`workflow_job_id`, and correlate against the delivery UUID for the original webhook delivery.

Historical spans use the same bounded non-blocking trace queue as everything else — the request
path never waits for export, and collector failures never change the authenticated
`204 No Content` response, readiness, generic webhook metrics, or merge-queue state.
