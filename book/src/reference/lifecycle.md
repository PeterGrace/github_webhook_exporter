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

One background task starts every pruning workload each `GHE_DELIVERY_PRUNE_INTERVAL_SECONDS`
(default `3600`); there is no separate queue prune interval. There is no immediate startup pass,
and missed ticks are skipped rather than replayed in a burst. Each scheduled pass fixes its cutoffs
once, then each SQLite operation deletes at most 1,000 eligible rows, repeating until one deletes
fewer than 1,000. Delivery pruning preserves fresh claims; queue pruning deletes only attempts with
a non-null completion time older than its cutoff, preserving fresh completed and all pending
attempts. Pruning runs in a fixed order: deliveries, merge-queue attempts, workflow-run contexts,
workflow-job links, the branch-protection required-check cache, in-job usage entries, vendor runs,
run-cost schedules and totals, per-pull-request run totals, pull-request cost records, then the
commit-to-pull-request map. Workflow, usage and run-cost rows and the commit map are keyed to
deliveries and runs and share the delivery cutoff. Vendor runs, per-pull-request run totals and
pull-request cost records share the `GHE_PR_COST_RETENTION_DAYS` cutoff (default `90`), measured
from each row's last update; rows of a pull request whose cost record is still pending are kept,
and a pending record is never pruned. The required-check cache additionally expires on read after
`GHE_REQUIRED_CHECK_TTL_SECONDS`, so pruning only reclaims space rather than governing
correctness.

A database failure stops only the affected workload's current pass. Normalized structured logging
identifies the workload and outcome and includes an opaque correlation ID for failures; it excludes
SQL text, row identifiers, repository identities, pull-request numbers, timestamps, and payload
data. The next configured interval retries both workloads.

During uninterrupted operation, a repeated delivery UUID is accepted but changes only request and
duplicate metrics. Queue state and specialized metrics update at most once after the durable
delivery-claim boundary. A process crash after a delivery or queue-state commit but before the
corresponding in-memory metric update can undercount it — SQLite and Prometheus are not
transactionally coupled, and the service does not promise exactly-once metrics across crashes.

## Branch-protection required-check refresh

When GitHub App credentials are configured (see
[Environment variables](environment-variables.md#branch-protection-required-checks)), a second
background task fills the required-check cache. It is entirely off the request path: webhook
handlers only ever read the cache, and a miss simply queues a refresh request and reports the
job's required status as unknown. The queue is bounded, and a request that arrives while it is full
is dropped rather than applying backpressure to an inbound delivery.

Each refresh re-checks the cache before calling GitHub, so a burst of deliveries for one branch
costs one API read. One short-lived installation token is minted on demand and reused until shortly
before it expires. Every failure — an unusable credential, a permission error, a rate limit, an
unreachable API, or a cache write failure — is logged as a bounded structured warning naming only
the outcome, and leaves the branch unknown. None of them can fail a webhook.

Without configured credentials this task never starts, and every workflow job reports its required
status as unknown. That is a supported deployment, not a degraded one.

## Run cost finalization

When a `costs` table or `ingest.oidc` is configured, a third background task totals each
completed run once its `GHE_RUN_GRACE_SECONDS` grace period has elapsed (see
[Run cost totals](traces.md#run-cost-totals)). It polls SQLite every 15 seconds, at once on
startup, and totals at most 100 due run attempts per poll, so a run is totalled between the end of
its grace period and one poll later. Missed polls are skipped.

Pending runs are stored in SQLite rather than held in memory, so a restart during a grace period
loses nothing: the next process totals every run that came due while it was down on its first
poll. Each total is written once, in one transaction with its per-pull-request row, and its span
and log are emitted only by the poll that wrote it. A storage failure leaves the run pending and
pushes its next try back, by 30 seconds after the first failure and doubling up to one hour, so a
run that keeps failing cannot crowd others out of a poll; retention removes it if it never
succeeds. A pending row that no longer parses is set aside and not polled again. The task makes
no GitHub API calls.

Without a `costs` table or `ingest.oidc` the task never starts and completed runs are not
scheduled.

## Pull request cost finalization

With the same configuration, a fourth background task finalizes each pull request's cost record
once one of the rules in [Pull request cost records](traces.md#pull-request-cost-records)
applies. It polls SQLite every 60 seconds, at once on startup, and finalizes at most 100 pull
requests per poll, honoring shutdown between them. Pending pull requests are stored in SQLite, so
one that comes due while the process is down is finalized by the next process. Each record is
built and marked final in one transaction, and its span and log are emitted only by the poll that
wrote it. A storage failure leaves the pull request pending for the next poll.

## Graceful shutdown

Tokio listens for both `SIGINT` and `SIGTERM`. Either signal follows the same sequence:

1. Record the normalized signal in structured stderr logging.
2. Notify Axum, the shared retention task, the required-check refresher, and the run-cost and
   pull-request cost finalizers through one cancellation signal.
3. Stop accepting new connections, stop scheduling new prune batches, stop serving new
   required-check refresh requests, and stop starting new run totals.
4. Allow active requests, active SQLite prune work, and a run total already being written to
   finish within one shared `GHE_SHUTDOWN_TIMEOUT_SECONDS` deadline. Runs not yet totalled stay
   pending for the next start.
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
