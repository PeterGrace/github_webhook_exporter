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
| `GHE_REQUIRED_CHECK_TTL_SECONDS` | `300` | How long a cached branch-protection answer stays confident; integer in `1..=86400`. |
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
| `SENTRY_DSN` | unset | Enables linked application-generated errors for failed/timed-out workflow tasks. Their Sentry mechanism is handled and omits the protocol-level `synthetic` field. Requires trace export and must target the same Sentry project as the OTLP trace endpoint. |

## Branch-protection required checks

These are the service's only *outbound* GitHub credentials. Everything else authenticates inbound
webhooks. Leave them unset and the feature is simply off: the required-check cache is never filled,
and every workflow job reports its required status as unknown. See
[Traces](traces.md#branch-protection-required-checks) for what the lookups produce.

Supplying some but not all of `GHE_GITHUB_APP_ID`, `GHE_GITHUB_APP_INSTALLATION_ID`, and a private
key is fatal at startup rather than a silent fallback to "disabled".

| Variable | Default | Contract |
| --- | --- | --- |
| `GHE_GITHUB_APP_ID` | unset | Numeric GitHub App identifier; positive integer. |
| `GHE_GITHUB_APP_INSTALLATION_ID` | unset | Numeric installation identifier; positive integer. |
| `GHE_GITHUB_APP_PRIVATE_KEY_PATH` | unset | Path to a mounted PEM private key, read verbatim. Takes precedence over `GHE_GITHUB_APP_PRIVATE_KEY`. |
| `GHE_GITHUB_APP_PRIVATE_KEY` | unset | Base64 encoding of the PEM private key, for deployments that pass it through the environment. |
| `GHE_GITHUB_API_BASE_URL` | `https://api.github.com` | REST API base URL; `http` or `https`. Set this for GitHub Enterprise Server (for example `https://ghe.example.com/api/v3`). A trailing slash is trimmed. |

The installation needs read access to branch protection (`administration: read`) on every
repository whose required checks you want resolved. The App private key is never logged: `Debug`
output for configuration renders it as `[REDACTED]`.

Do not place `GHE_GITHUB_APP_PRIVATE_KEY` in image arguments, labels, Dockerfiles, or committed
manifests. Prefer `GHE_GITHUB_APP_PRIVATE_KEY_PATH` with a mounted secret, which keeps the key off
the process environment entirely.

Structured logging to stderr is always on, independent of OTLP configuration.

Do not place secret values (`GHE_MASTER_KEY`, `GHE_ADMIN_TOKEN`, `SENTRY_DSN`, OTLP header values) in image
arguments, labels, Dockerfiles, or committed manifests.
