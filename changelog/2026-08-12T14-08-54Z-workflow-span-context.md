# Workflow OTLP span context and Actions links

- Project authenticated `workflow_run` deliveries into bounded trigger-event and sanitized branch
  metadata keyed by repository, workflow run, and run attempt.
- Persist only the correlation metadata in SQLite and prune it with the processed-delivery
  retention cutoff.
- Correlate completed `workflow_job` deliveries with durable run context across reruns and process
  restarts.
- Add `github.workflow.event`, `github.workflow.source_branch`, and
  `github.workflow.target_branch` to job and step spans when available.
- Add a derived `github.workflow.job.url` to job roots and a derived
  `github.workflow.step.url` with GitHub's step-log anchor to each step span.
- Continue ignoring payload-provided URLs and keep branches, context, and derived links out of logs
  and Prometheus labels.
- Add projection, storage, retention, webhook, workflow-emitter, and end-to-end OTLP coverage for
  pull-request and merge-queue executions.
