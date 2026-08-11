# GitHub Webhook Exporter

GitHub Webhook Exporter is a single-instance Rust service that turns GitHub webhook deliveries
into Prometheus metrics and OpenTelemetry traces, without persisting payloads. Each configured
repository authenticates with its own webhook secret; deliveries are verified, counted into
bounded-cardinality metrics, and discarded.

This site is organized around what you're trying to do, not around the source tree:

- **[Tutorials](tutorials/getting-started.md)** teach you the service by having you run it. Start
  here if you have never used it before.
- **How-to guides** ([Deploy with Helm](how-to/deploy-with-helm.md) and others) assume you already
  know the basics and want to accomplish a specific operational task, such as upgrading a live
  deployment or rotating a backup.
- **Reference** ([environment variables](reference/environment-variables.md),
  [HTTP API](reference/http-api.md), [metrics](reference/metrics.md), and more) is what you consult
  while working — precise, exhaustive, and not meant to be read start to finish.
- **[Explanation](explanation/architecture.md)** covers why the service is built the way it is:
  the single-instance model, bounded cardinality, and the privacy guarantees around webhook
  payloads.

The [Helm chart README](https://github.com/PeterGrace/github_webhook_exporter/blob/main/charts/github-webhook-exporter/README.md)
remains the authoritative reference for every `values.yaml` field; this site's
[Helm values](reference/helm-values.md) page is a map into it, not a replacement.
