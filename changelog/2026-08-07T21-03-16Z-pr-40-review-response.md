# PR 40 review response

- Added direct metric assertions for SDK `internal`, `timeout`, and `shutdown` failure mapping.
- Added a regression proving an HTTP-classified export failure is not also counted as `internal`.
- Exercised `http_response`, `timeout`, and `transport` through the complete observing HTTP client.
- Shared pipeline metrics with the webhook fixture and proved collector refusal increments exact
  Prometheus totals without changing authenticated responses or readiness.
- Extended blocked-exporter saturation coverage to assert exact trace/log `queue_full` metric totals
  and prove direct diagnostics do not recursively enter captured OTLP logs.
- Verified from OpenTelemetry SDK 0.32 worker behavior that pending exports are drained through the
  guarded `export` path before the structurally separate exporter shutdown call.
