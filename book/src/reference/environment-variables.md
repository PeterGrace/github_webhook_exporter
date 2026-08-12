# Environment variables

The Helm chart sets these for you through a generated ConfigMap and a Secret you supply. They
matter directly when running the container or binary yourself.

## Required — startup fails if any is missing or invalid

| Variable | Contract |
| --- | --- |
| `GHE_DATABASE_PATH` | Writable SQLite file path. |
| `GHE_MASTER_KEY` | Base64 encoding of exactly 32 random bytes. Derives the repository-secret encryption key. |
| `GHE_ADMIN_TOKEN` | Non-empty bearer token for the admin API. |

## Optional application settings

| Variable | Default | Contract |
| --- | --- | --- |
| `GHE_BIND_ADDRESS` | `[::]:8080` | Listener address. |
| `GHE_SHUTDOWN_TIMEOUT_SECONDS` | `30` | Drain deadline; positive integer. |
| `GHE_WEBHOOK_BODY_LIMIT_BYTES` | `2097152` | Maximum webhook body; this default is also the enforced maximum. |
| `GHE_WORKFLOW_JOB_MAX_STEPS` | `256` | Step cap for `workflow_job` traces; integer in `1..=1024`. No unlimited mode. |
| `GHE_DELIVERY_RETENTION_DAYS` | `7` | Delivery-ID retention; positive integer. |
| `GHE_MERGE_QUEUE_RETENTION_DAYS` | `90` | Completed merge-queue attempt retention; positive integer. |
| `GHE_DELIVERY_PRUNE_INTERVAL_SECONDS` | `3600` | Retention sweep interval; positive integer. |
| `GHE_OTEL_QUEUE_CAPACITY` | `2048` | Bounded export queue capacity, per enabled signal. |
| `GHE_OTEL_BATCH_SIZE` | `512` | Export batch size; cannot exceed queue capacity. |
| `GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS` | `5` | Telemetry flush deadline. |
| `RUST_LOG` | `info` | `tracing_subscriber` filter directive. |

## OpenTelemetry export

Export is entirely off unless at least one endpoint variable below is set. See
[Remote telemetry export](telemetry.md) for the pipeline these variables configure and
[How to configure remote telemetry](../how-to/configure-remote-telemetry.md) for setup steps.

| Variable | Default | Contract |
| --- | --- | --- |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | unset | Base OTLP/HTTP endpoint. `v1/traces` and `v1/logs` are appended. |
| `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` | derived | Complete trace endpoint, including `/v1/traces`. |
| `OTEL_EXPORTER_OTLP_LOGS_ENDPOINT` | derived | Complete log endpoint, including `/v1/logs`. |
| `OTEL_EXPORTER_OTLP_HEADERS` | unset | Headers for both signals. Percent-decoded and validated. |
| `OTEL_EXPORTER_OTLP_TRACES_HEADERS` | inherits generic | An explicitly empty value clears inherited headers. |
| `OTEL_EXPORTER_OTLP_LOGS_HEADERS` | inherits generic | An explicitly empty value clears inherited headers. |
| `OTEL_EXPORTER_OTLP_TIMEOUT` | `10000` | Export timeout, milliseconds. |
| `OTEL_EXPORTER_OTLP_TRACES_TIMEOUT` | inherits generic | Trace export timeout, milliseconds. |
| `OTEL_EXPORTER_OTLP_LOGS_TIMEOUT` | inherits generic | Log export timeout, milliseconds. |
| `OTEL_SERVICE_NAME` | `github-webhook-exporter` | Reported service name. |
| `OTEL_RESOURCE_ATTRIBUTES` | unset | Comma-separated `key=value`. Only `k8s.pod.name` and `k8s.namespace.name` are retained; other keys are dropped. Malformed entries are fatal at startup. |
| `SENTRY_DSN` | unset | Enables linked synthetic errors for failed/timed-out workflow tasks. Requires trace export and must target the same Sentry project as the OTLP trace endpoint. |

Structured logging to stderr is always on, independent of OTLP configuration.

Do not place secret values (`GHE_MASTER_KEY`, `GHE_ADMIN_TOKEN`, `SENTRY_DSN`, OTLP header values) in image
arguments, labels, Dockerfiles, or committed manifests.
