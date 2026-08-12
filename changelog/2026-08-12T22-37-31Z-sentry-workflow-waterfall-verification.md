# Verify Sentry workflow failure highlighting

- Verified successful, failed, and timed-out historical job and step spans end to end against
  Sentry SaaS using test marker `issue-79-1786572316`.
- Confirmed success ingests as `sentry.status=ok`; failure and timeout ingest as
  `sentry.status=error` and render as red-hatched waterfall lines.
- Confirmed `sentry.status_code` remains absent, avoiding a semantically false HTTP status on CI
  task spans.
- Documented Sentry's `x-sentry-auth` OTLP header and added a credential-free `.env.example`.
