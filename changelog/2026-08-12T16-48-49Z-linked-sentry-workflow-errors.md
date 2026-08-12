# Linked Sentry workflow errors and OTLP exception events

- Added canonical OpenTelemetry `exception` span events for failed and timed-out workflow tasks.
- Added optional `SENTRY_DSN` promotion for the same failures into synthetic Sentry errors linked to historical GitHub Actions spans.
- Emit a job-level fallback only when no failed or timed-out child step explains a failed job.
- Emit one handled synthetic error for each failed or timed-out workflow step, using the exact task span trace and span IDs.
- Include the sanitized task name in bounded exception descriptions and use stable repository/workflow/job/task fingerprints.
- Added Helm Secret projection through `existingSecret.keys.sentryDsn`, ignored local `.env` files, and documented setup, safety boundaries, and grouping.
- Keep error submission non-blocking through Sentry's bounded transport and include it in telemetry flush and shutdown.
- Added unit coverage for configuration validation/redaction, task-name fallbacks, Sentry event payloads, trace linkage, grouping, and duplicate suppression.
