# Attribute `workflow_run`-triggered runs to pull requests

Date: 2026-11-13T19:30:00Z

## What changed

- `workflow_run` joins the workflow event vocabulary, so a run started when another workflow
  completes reports `github.workflow.event=workflow_run` instead of `other`.
- Like comment-triggered runs, a `workflow_run`-triggered run has an empty `pull_requests`, so its
  pull request is read from the first `PR #<number>` in its run name. Set `run-name` on such
  workflows to attribute their jobs to a PR.
- Migration `202611130001` rebuilds `workflow_run_contexts` to accept the new event. Existing rows,
  including their pull request and workflow file, are copied.
