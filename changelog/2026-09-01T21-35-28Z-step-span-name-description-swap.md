# Step span name and `sentry.description` swapped

Reverses the previously documented contract for `github.actions.step` spans (issue #96).

- A step span's **name** is now the compound `<workflow-name> / <job-name> / <step-name>`, so
  Sentry waterfall rows carry the full path instead of a bare, ambiguous step name.
- A step span's `sentry.description` attribute is now the bare `<step-name>`.
- The two helpers in `src/telemetry/workflow.rs` kept their meanings and swapped their names, so
  `step_span_name` still names the span and `step_description` still fills the description.
- The linked synthetic Sentry error for a failed or timed-out step follows its span, so its trace
  context description is now the bare step name. This preserves the documented invariant that
  these errors "use the same bounded descriptions as the linked task spans" without a special
  case, and costs nothing for grouping, which is fingerprint-driven and unchanged.
- Job (`github.actions.job`) root spans, pipeline-run summary spans, `sentry.op` values, and every
  other step attribute are untouched.

This deliberately supersedes the decision recorded in
`changelog/2026-08-14T12-49-40Z-pr-review-follow-up.md`, which confirmed bare step span names as
intentional. `book/src/reference/traces.md` is updated to match.
