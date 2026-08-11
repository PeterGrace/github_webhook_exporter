# OTLP HTTPS and Failure Diagnostics Design

## Goal

Ensure configured HTTPS OTLP trace and log endpoints work in the production binary and make every
failed export locally diagnosable without exposing endpoint URLs, headers, payloads, response
bodies, or raw dependency errors.

## Root cause

The production `reqwest` dependency disables default features and enables only `blocking`. The
resolved dependency graph therefore contains no TLS backend. HTTPS OTLP attempts fail before an
HTTP response exists and are reduced to the broad `transport` reason. Curl succeeds from the same
pod network because curl has an independent TLS implementation.

The existing observing HTTP client records a bounded reason but discards safe structured metadata
available from `reqwest::Error`. Its one-minute per-category limiter can also hide individual
failed export attempts.

## Dependency change

Enable the `rustls` feature on the existing direct `reqwest` dependency while retaining
`default-features = false` and `blocking`. Do not replace the OpenTelemetry HTTP backend or add a
second HTTP client. Rustls avoids requiring OpenSSL packages in the minimal runtime image.

Add a regression test that exercises the production HTTP-client construction path and proves an
HTTPS request proceeds to connection/TLS handling rather than failing because HTTPS support is
absent. The test must remain local and must not depend on Sentry or public network availability.

## Failure diagnostics

Continue writing exporter diagnostics directly to stderr so failed log export cannot recursively
create another OTLP log. Every export failure emits one line; export failures are no longer
rate-limited. Queue-drop diagnostics retain their existing one-minute per-signal/reason rate limit
because queue saturation can generate one event per rejected record.

Exporter failure lines retain the fixed fields `kind`, `signal`, and `reason` and may add only these
bounded fields:

- `status=<three-digit code>` when `reqwest::Error::status` is available;
- `detail=connect` for connection establishment failures;
- `detail=request_builder` for request construction failures;
- `detail=redirect` for redirect-policy failures;
- `detail=request` for other request failures; and
- no detail when the SDK failure does not expose safe structured metadata.

Timeout remains represented by `reason=timeout`; malformed successful OTLP responses remain
`reason=encoding`. Classification uses only typed `reqwest::Error` predicates and status values.
Raw `Display`, `Debug`, and source-chain text must never enter diagnostics because those values can
contain endpoint or dependency details.

Prometheus counter names and bounded labels remain unchanged. Each failed attempt still increments
exactly one existing failure series.

## Data flow

1. OpenTelemetry submits an encoded batch through the existing blocking HTTP exporter thread.
2. The observing client delegates to the same `opentelemetry-http` reqwest implementation.
3. On failure, it derives the existing reason plus optional bounded status/detail metadata.
4. The diagnostics observer increments the existing Prometheus series and writes one direct stderr
   line.
5. The original error returns to OpenTelemetry unchanged; webhook responses and readiness remain
   independent of collector state.

## Testing

Unit tests cover exact diagnostic output for HTTP status and each supported normalized transport
detail, verify every export failure is emitted, and prove queue-drop rate limiting remains intact.
HTTP-client tests cover typed classification without asserting raw error strings.

A local HTTPS regression test proves TLS support is compiled into the same reqwest client used by
both OTLP signal exporters. Existing OTLP outage, HTTP-response, recursion, privacy, readiness, and
shutdown tests must continue to pass. Tests scan diagnostics to ensure configured endpoint,
credentials, collector response bodies, and raw transport text remain absent.

## Documentation and scope

Update the remote-telemetry operations reference to state that HTTPS uses rustls and describe the
new bounded fields and per-export reporting behavior. Add a timestamped changelog record.

This change does not add retries, alter queue capacity, change exporter timeouts, expose response
bodies, introduce Sentry-specific behavior, or replace the OpenTelemetry HTTP backend.
