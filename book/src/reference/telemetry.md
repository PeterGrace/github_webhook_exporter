# Remote telemetry export

Structured stderr logging is always active. Remote trace and log export is optional and starts
only when `OTEL_EXPORTER_OTLP_ENDPOINT`, `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT`, or
`OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` is set — with none set, no remote provider or export queue is
created, and local logging remains fully functional. See
[How to configure remote telemetry](../how-to/configure-remote-telemetry.md) for setup and
[Environment variables](environment-variables.md) for every `OTEL_*`/`GHE_OTEL_*` variable.

`OTEL_SERVICE_NAME` defaults to `github-webhook-exporter`; every resource includes the package
version. Of the values in `OTEL_RESOURCE_ATTRIBUTES`, only `k8s.pod.name` and
`k8s.namespace.name` are retained. Invalid requested telemetry configuration fails startup with
only the variable name. Collector latency or unavailability happens on dedicated exporter threads
and never changes HTTP readiness or request results.

## Queue and batching

Each enabled signal (`trace`, `log`) uses its own non-blocking bounded queue.

| Variable | Default | Contract |
| --- | --- | --- |
| `GHE_OTEL_QUEUE_CAPACITY` | `2048` | Maximum admitted records per signal. |
| `GHE_OTEL_BATCH_SIZE` | `512` | Maximum records per export request; no greater than queue capacity. |
| `GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS` | `5` | One shared trace-and-log shutdown deadline. |

## Failure and drop accounting

| Metric | Labels |
| --- | --- |
| `github_telemetry_export_failures_total` | `signal`, `reason` |
| `github_telemetry_dropped_records_total` | `signal`, `reason` |

`signal` is exactly `trace` or `log`.

**Export-failure `reason`**: `transport`, `timeout`, `http_response`, `encoding`, `shutdown`,
`internal`, `other`. A malformed successful collector response is counted as `encoding`. Collector
response bodies and transport error text are never exposed.

**Drop `reason`**: `queue_full` or `pipeline_closed`.

All bounded series exist at zero before the first event. Every failed export writes one direct
stderr line and increments one bounded series. HTTP failures may include `status=<code>`;
transport failures may include only `detail=connect|request_builder|redirect|request`. Raw errors,
endpoint URLs, headers, credentials, request payloads, and collector response bodies are never
written — this path bypasses `tracing` and OpenTelemetry logs, so exporter diagnostics cannot
recursively enter the failing log pipeline. Queue-drop diagnostics are limited to one line per
signal/reason category per monotonic minute; repeated drops still increment Prometheus, and the
next permitted line reports how many local lines were suppressed.

## What to alert on

Sustained increases in
`github_telemetry_export_failures_total{reason=~"transport|timeout|http_response"}` indicate
collector reachability, latency, or response failures. `encoding` and `internal` indicate
compatibility or implementation faults; an unexpected `shutdown` reason indicates lifecycle
misordering. A rising `github_telemetry_dropped_records_total{reason="queue_full"}` is primarily a
capacity/tuning signal under load; `pipeline_closed` before a planned shutdown indicates incorrect
producer lifecycle ordering.

## Shutdown interaction

On shutdown, application admission closes before provider workers disconnect. Accepted records
that finish exporting release their pending slots; slots still pending when shutdown finishes or
reaches its deadline are atomically counted as `pipeline_closed` drops. A finalization gate
prevents a late shutdown worker from exporting a batch after its slots were already counted as
dropped, and later records are rejected and counted the same way. Trace and log provider shutdown
begin concurrently and share the single `GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS` deadline. A failed
provider is counted with normalized reason `shutdown`; a provider unfinished at the deadline is
counted with reason `timeout`. Either condition uses the same direct, redacted stderr diagnostic
path and never turns a successful HTTP drain into a process failure. See
[Startup, retention, and shutdown](lifecycle.md) for the full shutdown sequence.

## Identifiers

Delivery, pull-request, commit, workflow, job, and step identifiers remain span-only — see
[Traces](traces.md). Canonical repository names additionally appear on repository-scoped
Prometheus series. None of these identifiers appears in local or OTLP application logs, except the
one bounded workflow-rejection warning documented in [Traces](traces.md#completed-workflow-traces).
