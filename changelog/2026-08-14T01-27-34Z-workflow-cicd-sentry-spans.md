# Workflow CI/CD and Sentry span enrichment

- Replaced fixed workflow job and step span names with bounded descriptive names and fixed fallbacks.
- Marked workflow task spans as `INTERNAL` and added explicit `github.actions.job` and
  `github.actions.step` Sentry operations and descriptions.
- Applied CI/CD task-run and VCS semantic-convention attributes to every workflow task span.
- Replaced workflow-specific GitHub repository, commit, run, job, and URL attributes with their
  standard equivalents.
- Added bounded task results for every conclusion and `error.type` for failures and timeouts.
- Aligned linked Sentry error trace contexts with the corresponding task span operation and
  description.
- Updated workflow, OTLP, privacy, conclusion, hierarchy, and Sentry linkage tests and trace
  reference documentation.
- Verified live Sentry ingestion with marker `issue-86-1786711273`: waterfall rows use the
  descriptive job and step names, operations are `github.actions.job` and `github.actions.step`,
  success is `ok`, failure and timeout are `error`, and both linked errors use the exact failed or
  timed-out task span IDs in the same trace.
