# Startup, retention, and shutdown

## Startup

The service validates configuration and initializes local structured stderr logging before
opening SQLite. It then opens the configured database, applies embedded migrations, constructs
the router, and binds the HTTP listener, in that order.

A configuration, database-open, migration, or initial repository-count failure is fatal. Before
binding, the service initializes `github_repository_configurations` from the durable repository
count, so readiness is never served with a default gauge value. Successful API creates and deletes
adjust the in-memory gauge after their SQLite commits — a forced process exit in that narrow gap
can leave the final in-process value stale, but the next startup reconciles it from durable state.
The process exits nonzero on any startup failure and cannot report false readiness. Local errors
identify the failed stage without including configured credentials or the database path.

## Delivery and merge-queue retention

The service retains authenticated delivery claims for `GHE_DELIVERY_RETENTION_DAYS` (default `7`)
and completed pull-request merge-queue attempts for `GHE_MERGE_QUEUE_RETENTION_DAYS` (default
`90`). Both must be positive integers. Pending merge-queue attempts are retained regardless of age
so a later completion can correlate across a restart.

One background task starts both pruning workloads every `GHE_DELIVERY_PRUNE_INTERVAL_SECONDS`
(default `3600`); there is no separate queue prune interval. There is no immediate startup pass,
and missed ticks are skipped rather than replayed in a burst. Each scheduled pass fixes its cutoffs
once, then each SQLite operation deletes at most 1,000 eligible rows, repeating until one deletes
fewer than 1,000. Delivery pruning preserves fresh claims; queue pruning deletes only attempts with
a non-null completion time older than its cutoff, preserving fresh completed and all pending
attempts.

A database failure stops only the affected workload's current pass. Normalized structured logging
identifies the workload and outcome and includes an opaque correlation ID for failures; it excludes
SQL text, row identifiers, repository identities, pull-request numbers, timestamps, and payload
data. The next configured interval retries both workloads.

During uninterrupted operation, a repeated delivery UUID is accepted but changes only request and
duplicate metrics. Queue state and specialized metrics update at most once after the durable
delivery-claim boundary. A process crash after a delivery or queue-state commit but before the
corresponding in-memory metric update can undercount it — SQLite and Prometheus are not
transactionally coupled, and the service does not promise exactly-once metrics across crashes.

## Graceful shutdown

Tokio listens for both `SIGINT` and `SIGTERM`. Either signal follows the same sequence:

1. Record the normalized signal in structured stderr logging.
2. Notify both Axum and the shared retention task through one cancellation signal.
3. Stop accepting new connections and stop scheduling new delivery or queue prune batches.
4. Allow active requests and active SQLite prune work to finish within one shared
   `GHE_SHUTDOWN_TIMEOUT_SECONDS` deadline.
5. Record that telemetry provider shutdown is starting, close both telemetry admission boundaries,
   and begin trace and log provider shutdown concurrently.
6. Wait at most the separate shared `GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS` deadline for both
   providers.
7. Exit normally after a successful HTTP/retention drain even when telemetry shutdown fails or
   times out, emitting only normalized direct diagnostics and bounded Prometheus accounting.
8. Preserve a pre-existing startup or server error after telemetry cleanup, instead of replacing it
   with a provider error.

`GHE_SHUTDOWN_TIMEOUT_SECONDS` defaults to `30` and must be a positive integer:

```bash
GHE_SHUTDOWN_TIMEOUT_SECONDS=5 github_webhook_exporter
```

Service managers should send `SIGTERM` and configure their termination grace period to exceed the
sum of `GHE_SHUTDOWN_TIMEOUT_SECONDS` and `GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS`, plus a small
process-exit allowance. `SIGINT` is primarily useful for an interactive local run. Startup failures
after telemetry initialization and server errors use the same final provider cleanup path.
Repeated runtime shutdown calls are idempotent and never invoke either provider twice.

Lifecycle logs contain the bound address, normalized signal kind, completion, and timeout
duration. They do not log runtime configuration, authorization headers, request bodies,
encryption material, or repository secrets.
