# Workflow-job step-limit operator contract

Documented and finalized the bounded completed-workflow admission contract.

- Added the operator-facing `GHE_WORKFLOW_JOB_MAX_STEPS` contract to `docs/operations.md` with
  the default `256`, the accepted range `1..=1024`, and the explicit absence of an unlimited mode.
- Documented the bounded count-only admission pass that inspects structurally valid completed jobs
  before detailed projection or historical trace emission.
- Documented all-or-nothing rejection: accepted jobs emit every reported step, while over-limit
  jobs remain durably claimed, return `204 No Content`, and emit no partial workflow trace.
- Documented the exact unlabeled histogram `github_workflow_job_steps`, its bucket boundaries
  (`0`, `5`, `10`, `20`, `40`, `64`, `128`, `256`, `512`, `1024`, plus `+Inf`), and the bounded
  rejection counter `github_workflow_job_trace_rejections_total{reason="too_many_steps"}`.
- Documented the actionable parentless rejection warning, including the only approved identifier
  fields (`repository_name`, `delivery_id`, `workflow_run_id`, `workflow_run_attempt`,
  `workflow_job_id`, `step_count`, `step_limit`) and the retained boundary that other workflow
  identifiers and names remain span-only.
- Documented the GitHub Actions job lookup procedure using
  `GET /repos/{owner}/{repo}/actions/jobs/{job_id}` with the canonical repository name,
  `workflow_job_id`, and delivery UUID correlation.
- Captured that the implementation is covered by tests for limit parsing, bounded metrics,
  end-to-end acceptance and rejection behavior, structured-log and OTLP privacy, and duplicate
  suppression.
