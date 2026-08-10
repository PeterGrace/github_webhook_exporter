# Example Grafana dashboard

- Added the standalone Grafana 10+ dashboard at
  `examples/grafana/github-webhook-exporter.json`.
- Added Prometheus datasource, job, and instance variables with multi-value and `All` filtering.
- Added operational overview, webhook, merge-queue, workflow-job, and telemetry panels covering all
  15 emitted Prometheus metric families.
- Added `tests/grafana_dashboard.rs` to validate JSON structure, stable identity, variables, rows,
  datasource and selector use, and complete metric-family query coverage.
- Added import and customization guidance at `examples/grafana/README.md`.
- Validated with the focused dashboard test, `just fmt`, Clippy with warnings denied, the complete
  test suite, Python's JSON parser, and Git whitespace checks.
