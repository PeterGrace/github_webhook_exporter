# Example Grafana dashboard

This directory contains an importable Grafana 10+ dashboard for all Prometheus metric families
emitted by the GitHub Webhook Exporter.

## Import

1. Configure Prometheus to scrape the exporter's public `GET /metrics` endpoint. Helm users may
   enable `metrics.service.enabled` and `metrics.serviceMonitor.enabled`; see the
   [chart documentation](../../charts/github-webhook-exporter/README.md) for configuration details.
2. In Grafana, choose **Dashboards > New > Import** and upload
   [`github-webhook-exporter.json`](github-webhook-exporter.json).
3. Select a Prometheus datasource when prompted.
4. Use the dashboard's **Job**, **Instance**, and **Repository** filters to select one or more
   exporter series. All three filters support **All**.

The Repository filter contains canonical `owner/repository` names and the synthetic `unknown`
value. Select `unknown` to inspect request outcomes and failures for which authentication did not
establish repository identity. Configured-repository and telemetry-health panels remain global to
the selected exporter job and instances because those metric families do not carry a repository
label.

The dashboard starts with a compact operational overview, followed by detailed webhook,
merge-queue, workflow-job, and telemetry sections. It uses Grafana's selected time range and rate
interval so operators can adapt it without changing fixed windows.

This dashboard is an editable starting point. It intentionally does not provide Grafana
provisioning, alert rules, Prometheus recording rules, or Helm-managed Grafana resources.
