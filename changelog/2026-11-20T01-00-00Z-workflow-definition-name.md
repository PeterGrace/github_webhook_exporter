# Name workflows by their definition, not their run name

Date: 2026-11-20T01:00:00Z

## What changed

- A workflow that sets `run-name` reports that run name in `workflow_run.name` and
  `workflow_job.workflow_name`. The exporter used those as the workflow name, so such a workflow
  split into one `github.workflow`, `cicd.pipeline.name` and span-description value per run, and
  its cost never added up under one name.
- The workflow name now comes from the top-level `workflow.name` of `workflow_run` deliveries,
  which a `run-name` does not change. Pipeline roots and `ci.run.cost` spans read it from the
  delivery, and job spans read it from the stored run context. The old fields remain the fallback
  when no context was stored or the delivery has no usable `workflow.name`.
- Migration `202611200002` adds a nullable, bounded `workflow_name` column to
  `workflow_run_contexts`.

## Operator action

None beyond the backup the pull-request cost records migration already asks for. Spans already
exported keep their old names, so a dashboard grouped by `github.workflow` shows both names for a
`run-name` workflow until the older spans age out.
