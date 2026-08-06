# Final tracing review fixes

- Filtered ordinary `tracing` events from the OTLP trace layer while preserving structured stderr
  and OTLP application-log export. The repository 500 regression now enforces the span-event
  allowlist and confirms the internal failure remains present in both log outputs.
- Explicitly dropped the `github.webhook.authenticate` span handle before constructing
  `github.webhook.process`, with exported timestamp ordering and subscriber close-before-create
  lifecycle coverage.
- Retained parsed OTLP log records in privacy tests and added direct attribute-key and integer-value
  checks for repository IDs, pull-request numbers, and retention-only identifiers. Added a
  regression demonstrating that protobuf varint values are not detectable as decimal text.
- Added a test-only one-pass retention seam shared with the production traced-pass implementation.
  Exact-one retention tests no longer depend on scheduler sleeps or polling, and ambient spans are
  constructed inside the subscriber-bound context so independent-root assertions detect parent
  regressions. Duplicate-delivery hierarchy checks now anchor at the unique update span instead of
  depending on nondeterministic OTLP export order.
- Restored private visibility for `QueueTransitionFailureReason::as_str` and replaced the trace
  module's broad dead-code allowance with a narrow test-only `Operation::as_str` implementation.
- Focused telemetry, retention, trace-policy, and metrics tests pass without warnings.
