# Architecture

GitHub Webhook Exporter is deliberately small: one Rust binary, one SQLite file, one HTTP
listener. Understanding why it's shaped that way makes the rest of the reference material easier
to reason about.

## A converter, not a store

The service's job is to convert a stream of GitHub webhook deliveries into two things Prometheus
and an OTLP collector already know how to consume: bounded-cardinality counters/histograms and
short-lived spans. It is not trying to be a webhook archive, an event bus, or a general-purpose
GitHub integration platform. That framing explains several choices that might otherwise look like
missing features — there's no query API over past deliveries, no webhook replay, and no payload
storage, because none of that is the job. See
[Why webhook payloads are never persisted](design-decisions.md#why-webhook-payloads-are-never-persisted)
for the privacy reasoning that reinforces this.

## Request lifecycle

Every request enters through Axum and takes one of two paths:

- **`GET /metrics`, `GET /health/*`** are read-only and unauthenticated; they exist for
  infrastructure (Prometheus, an orchestrator's probes) rather than for people.
- **`POST /webhooks/github`** authenticates against a repository-specific secret, claims the
  delivery ID atomically to guarantee at-most-once processing of duplicates, and only then updates
  metrics and, for a narrow set of event types, projects state into SQLite or a historical trace.
  Authentication happens before any business logic runs, so an unregistered or incorrectly signed
  caller can't influence application state, only the fixed `401` response.

The admin API (`/api/v1/repositories/*`) is a separate concern from webhook ingestion — it's how
an operator tells the service which repositories exist and what their secrets are, gated by a
single bearer token rather than per-repository credentials, because the trust model for
"can configure this service" is deliberately simpler than "can deliver webhooks for repository X."

Full request-by-request behavior, including every status code, is in [HTTP API](../reference/http-api.md).

## Why SQLite, and why a singleton

SQLite holds three things: repository configuration (including encrypted secrets), delivery IDs
for deduplication, and merge-queue attempt state. All three are small, all three need durable
writes on the request path, and none of them need to be queried by more than one process at a
time. A single-writer embedded database is a good fit for that, and it avoids operating a separate
database service for what is, in steady state, a modest amount of state.

The corollary is that the service runs as a fixed singleton — the Helm chart installs exactly one
StatefulSet replica, never more. A second writer against the same SQLite file isn't a supported
configuration, so horizontal scaling isn't offered as an option; if you need to distribute the work
of many repositories, run more Kubernetes resources' worth of headroom for one replica rather than
several replicas. This is also why the upgrade and backup/restore procedures in the how-to guides
are as careful as they are about ensuring only one writer ever holds the PVC — see
[How to upgrade a running deployment](../how-to/upgrade-a-deployment.md) and
[How to back up and restore SQLite](../how-to/back-up-and-restore.md).

## Two independent telemetry surfaces

Prometheus metrics and OTLP traces/logs are exported through entirely separate paths with
different failure semantics. Metrics are in-process counters that always reflect current state;
scraping them can't fail in a way that affects the service. OTLP export, by contrast, talks to a
network collector that may be slow, unreachable, or misconfigured — so it runs on dedicated
exporter threads behind a bounded, non-blocking queue, and a collector outage degrades to dropped
telemetry rather than backpressure on webhook processing. See
[Remote telemetry export](../reference/telemetry.md) for the queue and failure-accounting contract
that implements this isolation, and
[Bounded cardinality and span-only identifiers](design-decisions.md#bounded-cardinality-and-span-only-identifiers)
for why the two surfaces carry different levels of detail about the same events.
