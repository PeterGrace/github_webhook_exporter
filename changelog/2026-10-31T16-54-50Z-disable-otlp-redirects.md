# Disable OTLP HTTP redirects

Date: 2026-10-31T16:54:50Z

## What changed

- Disabled redirects in the production OTLP HTTP client so custom exporter headers cannot be
  forwarded to a redirect target.
- Added a regression test that verifies a `307 Temporary Redirect` produces the bounded redirect
  diagnostic and never connects to the target listener.

This resolves the CLM-0195 security finding tracked by issue #111.
