# Example Grafana Dashboard Design

## Problem

The exporter exposes 15 bounded Prometheus metric families, but operators currently have no
repository-owned Grafana dashboard that turns those metrics into an immediately useful operational
view. Users must discover metric semantics and construct PromQL manually before they can assess
webhook health, merge-queue behavior, workflow-job shape, or exporter telemetry health.

## Scope

Add one portable, importable Grafana dashboard JSON file and a short companion README under
`examples/grafana/`. The dashboard is an MVP that provides an operational overview followed by
focused detail rows. It is an example and starting point, not a production alerting contract.

The change does not add Grafana provisioning, alert rules, recording rules, Helm integration, or a
dashboard-generation dependency.

## Compatibility and variables

The JSON targets Grafana 10 or newer and uses the standard Prometheus data source plugin. It defines
these template variables:

- `datasource`: a Prometheus data source selector.
- `job`: values discovered from `label_values(github_webhook_requests_total, job)`.
- `instance`: values discovered from
  `label_values(github_webhook_requests_total{job=~"$job"}, instance)`.
- `repository`: canonical repository names, plus the synthetic `unknown` pre-authentication value,
  discovered from
  `label_values(github_webhook_requests_total{job=~"$job", instance=~"$instance"}, repository)`.

The `job`, `instance`, and `repository` variables allow multiple selections and an `All` selection.
Every PromQL query applies `job=~"$job"` and `instance=~"$instance"`. Queries for repository-scoped
webhook, merge-queue, and workflow families additionally apply
`repository=~"$repository"`. Global repository-count and telemetry-health metrics do not carry the
repository label and therefore remain filtered only by job and instance. Queries use
`$__rate_interval` for rates and `$__range` for range totals where appropriate.

## Dashboard layout

### Operational overview

The first row gives a compact service-health view:

- webhook request rate by result;
- accepted-request percentage;
- p95 webhook processing latency;
- duplicate delivery rate;
- configured repository count;
- processing failure rate by stage;
- telemetry export failure rate; and
- dropped telemetry record rate.

### Webhook details

Panels show event rate by event/action, request-body size percentiles, request latency percentiles,
request totals by result, duplicate totals, and processing failure totals. This row covers:

- `github_webhook_requests_total`;
- `github_webhook_events_total`;
- `github_webhook_processing_duration_seconds`;
- `github_webhook_request_body_bytes`;
- `github_webhook_duplicates_total`;
- `github_webhook_processing_failures_total`; and
- `github_repository_configurations`.

### Merge-queue details

Panels show merge-group event rates, pull-request queue outcomes, p50/p95 attempt durations by
outcome, and transition failures by reason. This row covers:

- `github_merge_group_events_total`;
- `github_merge_queue_pr_outcomes_total`;
- `github_merge_queue_attempt_duration_seconds`; and
- `github_merge_queue_transition_failures_total`.

### Workflow and telemetry details

Panels show workflow-job step-count percentiles, trace rejection rates, OTLP export failures, and
dropped records, grouped by their bounded labels. This row covers:

- `github_workflow_job_steps`;
- `github_workflow_job_trace_rejections_total`;
- `github_telemetry_export_failures_total`; and
- `github_telemetry_dropped_records_total`.

## PromQL behavior

Counter panels use `rate()` for time-series throughput and `increase()` for selected-range totals.
Histogram percentiles use `histogram_quantile()` over rates of `_bucket` series while retaining the
metric's meaningful bounded labels. Repository-scoped aggregations filter by the selected repository
but need not retain `repository` in their output legends because the dashboard variable already
identifies the selected scope. Ratios guard against division by zero so idle exporters do not
produce misleading infinities. Gauge panels use the latest selected series without applying rate
functions.

The dashboard does not claim exactly-once event accounting across process crashes; it visualizes the
exporter's documented metric semantics.

## Documentation

`examples/grafana/README.md` explains how to enable scraping, import the dashboard, select the
Prometheus data source, and use the `job`, `instance`, and `repository` filters. It explains that
`unknown` represents requests for which authentication did not establish repository identity. It
identifies Grafana 10+ as the example target and states the explicit non-goals.

## Validation

A focused repository test parses the dashboard as JSON and verifies:

- the dashboard has the expected title and schema version;
- the four template variables exist with the expected dependency order;
- every one of the 15 emitted metric families appears in at least one query;
- all Prometheus query targets use the datasource variable and job/instance filters;
- repository-scoped queries use the repository filter while global queries do not; and
- the expected overview and detail rows exist.

The implementation also runs `just fmt`, `cargo clippy --all-targets -- -D warnings`, and
`just test`. JSON syntax is independently checked with an available JSON parser.
