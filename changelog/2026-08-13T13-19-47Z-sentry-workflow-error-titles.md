# Descriptive Sentry workflow error titles

- Omitted Sentry's protocol-level `mechanism.synthetic` field from application-generated GitHub
  Actions failure and timeout events.
- Preserved the handled `github_actions` mechanism, bounded exception type and value, stable
  fingerprint, historical trace/span linkage, tags, timestamp, and privacy boundary.
- Added focused and integration assertions that keep the mechanism field absent. Sentry can now
  retain the exception metadata and derive descriptive issue titles instead of `<unknown>`.
- Clarified telemetry documentation that these workflow errors are application-generated without
  claiming they are synthetic at the Sentry protocol level.
