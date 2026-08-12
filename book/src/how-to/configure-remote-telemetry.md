# How to configure remote telemetry

Structured stderr logging is always on and needs no configuration. Remote trace and log export
over OTLP/HTTP is optional and off by default — it starts only once you set one of the endpoint
variables below.

## Point at a single collector for both signals

```bash
export OTEL_EXPORTER_OTLP_ENDPOINT=https://collector.example.test:4318
export OTEL_EXPORTER_OTLP_TIMEOUT=10000
github_webhook_exporter
```

The generic endpoint gets `/v1/traces` and `/v1/logs` appended automatically. HTTPS endpoints use
the bundled `rustls` client, so you don't need OpenSSL in the runtime image.

## Route traces and logs to different collectors

Set each signal's endpoint to its *complete* URL, including the path:

```bash
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=https://traces.example.test:4318/v1/traces
export OTEL_EXPORTER_OTLP_LOGS_ENDPOINT=https://logs.example.test:4318/v1/logs
```

## Send auth headers to the collector

Keep the encoded header value out of scripts and shell history where you can — read it from the
operator's environment instead:

```bash
: "${OTEL_EXPORTER_OTLP_HEADERS:?set OTEL_EXPORTER_OTLP_HEADERS in the operator shell}"
export OTEL_EXPORTER_OTLP_HEADERS
```

Set a signal-specific header variable (`OTEL_EXPORTER_OTLP_TRACES_HEADERS` or
`_LOGS_HEADERS`) to override just that signal's headers, or set it to an explicitly empty value to
clear the inherited generic headers for that signal only.

## Link failed CI tasks to Sentry traces

Canonical OTLP trace export already carries bounded `exception` span events for failed and
timed-out workflow tasks; no Sentry configuration is required for that representation. `SENTRY_DSN`
optionally adds a parallel Sentry envelope for native Issue grouping, does not disable the OTLP
event, and there is no OTLP errors endpoint. Treat the OTLP event as the observable source of
record: the Sentry SDK does not report individual internal queue-overflow drops through its capture
API.

Configure trace export to the Sentry OTLP trace endpoint, authenticate OTLP with the public key
from the DSN, then provide the DSN for that same Sentry project:

```bash
: "${SENTRY_DSN:?set SENTRY_DSN in the operator shell}"
: "${SENTRY_PUBLIC_KEY:?set SENTRY_PUBLIC_KEY from the DSN}"
export SENTRY_DSN
export OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=https://o123.ingest.sentry.io/api/42/integration/otlp/v1/traces
export OTEL_EXPORTER_OTLP_TRACES_HEADERS="x-sentry-auth=sentry%20sentry_key=${SENTRY_PUBLIC_KEY}"
```

`%20` is decoded to the required space in the `x-sentry-auth` value. Do not use an
`Authorization: Bearer` header for direct Sentry OTLP ingestion.

The DSN enables synthetic handled errors for failed and timed-out workflow tasks. Each error uses
the historical task span's trace and span IDs, allowing Sentry to place it on that span in the
waterfall. Startup rejects `SENTRY_DSN` unless trace export is enabled. Keep the DSN out of scripts
and committed configuration.

## Under Helm

Add the header value as a key on the Secret you already created for `master-key` and
`admin-token`, then reference it from `existingSecret.*` — see the
[chart README](https://github.com/PeterGrace/github_webhook_exporter/blob/main/charts/github-webhook-exporter/README.md)
for the exact key names the chart projects as `OTEL_EXPORTER_OTLP_*` variables. To enable optional
linked CI-task errors, add the Sentry DSN to that Secret and set `existingSecret.keys.sentryDsn`
to its key name.

## Tune the export queue

Each enabled signal gets its own non-blocking bounded queue. Raise `GHE_OTEL_QUEUE_CAPACITY` if
you're seeing `queue_full` drops under load, and keep `GHE_OTEL_BATCH_SIZE` at or below it. See
[Remote telemetry export](../reference/telemetry.md) for defaults, the full failure/drop reason
vocabulary, and what to alert on.

## Confirm it's working

```bash
curl --silent http://localhost:8080/metrics | grep github_telemetry
```

`github_telemetry_export_failures_total` and `github_telemetry_dropped_records_total` should both
be absent or zero. If they're not, [Remote telemetry export](../reference/telemetry.md) explains
what each `reason` label means and which ones indicate a collector problem versus a tuning
problem.
