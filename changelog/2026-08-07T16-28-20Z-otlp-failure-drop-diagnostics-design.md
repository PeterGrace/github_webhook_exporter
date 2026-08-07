# OTLP failure and drop diagnostics design

- Defined bounded Prometheus dimensions for trace/log export failures and dropped records.
- Selected an observing OTLP HTTP client so failures are classified before the SDK erases structured
  transport and response information.
- Defined atomic per-category one-minute stderr limiting with suppression accounting and no
  recursive use of `tracing` or OpenTelemetry logs.
- Classified malformed successful OTLP responses as `encoding` failures.
- Preserved lock-free queue admission and complete independence from webhook responses and
  readiness.
