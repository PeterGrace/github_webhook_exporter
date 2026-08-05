# Core Service and Merge-Queue Tracing Design

## Goal

Instrument the live HTTP, webhook, repository, SQLite, merge-queue, and retention paths with a
bounded OpenTelemetry trace hierarchy. The change must preserve every existing response,
persistence, metric, deduplication, and redaction guarantee.

This design implements GitHub issue #34 on top of the bounded OTLP runtime from #33. The newer
Phase 4 issue contract supersedes the older blanket identifier prohibition in
`2026-08-03-04-otlp-observability.md`: approved diagnostic identifiers may appear only as
OpenTelemetry span attributes, never as ordinary `tracing` fields or application logs.

## Architecture

A focused telemetry policy module owns:

- stable operation names;
- attribute keys;
- bounded attribute enums and values;
- helpers that attach approved diagnostic identifiers directly to the current OpenTelemetry span;
- helpers for bounded span status and events.

Application code creates spans explicitly at meaningful operation boundaries. This is preferred to
broad `#[instrument]` annotations because function arguments include secrets, payloads, and raw
values that must never be captured. It is also preferred to automatic SQL instrumentation because
raw statements and database details are outside the allowlist.

## Span hierarchy

Each routed HTTP request creates an `http.request` span containing only:

- HTTP method;
- Axum matched route template;
- response status;
- normalized request result.

The request middleware obtains the matched route after the inner service completes, so it records a
route template rather than a raw URI. Unknown routes use a bounded fallback value.

Webhook requests nest these operations beneath the request span:

1. `github.webhook.authenticate` covers repository lookup and signature verification.
2. `github.webhook.process` covers authenticated projection, delivery claim, generic metrics, and
   specialized processing.
3. `sqlite.query` spans cover allowlisted store operation names.
4. `merge_queue.update` spans cover supported specialized merge-group and pull-request queue
   transitions.

Repository create, update, and delete handlers create `config.repository.write` children. Their
store calls create nested `sqlite.query` spans. Read-only repository operations receive only the
request and SQLite spans.

Each scheduled retention pass is an independent root rather than inheriting an unrelated request
context. Delivery and merge-queue prune operations are represented by bounded SQLite child spans.

## Attribute policy

Bounded operational attributes include:

- HTTP method, route template, and status;
- normalized request result;
- normalized webhook event type and action;
- allowlisted database operation;
- normalized merge-group reason;
- normalized queue outcome and reason.

Issue #34 records these diagnostic identifiers as span-only attributes when available:

- canonical repository name;
- repository ID;
- pull-request number;
- delivery ID;
- merge-group or pull-request SHA represented by the current core payload projection.

The helper reaches the active OpenTelemetry span through `tracing-opentelemetry` and calls the
OpenTelemetry span API directly. Identifier values are never declared as `tracing` span fields or
emitted as events, which keeps them out of structured stderr and OTLP application logs. This helper
is the sole policy seam for future changes.

Workflow run ID/attempt and workflow job ID remain the responsibility of issue #10. Its issue body
will be updated to identify that boundary and require reuse of the span-only helper.

Globally forbidden values remain forbidden everywhere: request bodies, payload fragments, secrets,
signatures, authorization and OTLP headers, actors, raw URLs, commands, raw actions, raw reasons,
and arbitrary SQL.

## Merge-queue and deduplication semantics

Specialized spans are emitted only after a delivery claim returns `New`. A duplicate claim therefore
has request, authentication, processing, and claim-query visibility but cannot emit another
specialized merge-group or pull-request transition span or outcome event.

Merge-group `destroyed` preserves bounded reasons, including authoritative `merged` and normalized
`dequeued`. Pull-request dequeue remains outcome `unknown` with reason
`unclassified_dequeue`. Existing persistence and Prometheus behavior remains unchanged.

Queue-state persistence failures still return authenticated `204`. The active specialized span
records a bounded failure event and error status; ordinary logs retain only their existing bounded
stage, result, and correlation ID fields.

## Error handling

HTTP status and normalized request result are recorded after response generation, including
extractor and middleware rejections. Authentication and database failures set bounded status on the
relevant operation span without recording error strings. Existing `AppError` conversion remains the
source of response status and body behavior.

Telemetry APIs are best-effort and cannot affect business results. Missing remote telemetry,
collector failure, or absent active OpenTelemetry context changes neither control flow nor returned
errors.

## Testing

Tests use the in-process OTLP/HTTP protobuf receiver and force provider flushes before assertions.
They verify:

- request parentage for webhook and repository API paths;
- authentication, processing, delivery claim, SQLite, merge-group, and PR queue descendants;
- repository-write spans and maintenance roots;
- success, malformed, unauthorized, unavailable, and queue-state failure status;
- duplicate deliveries omit repeated specialized transition spans and events;
- group `dequeued` and PR `unknown/unclassified_dequeue` semantics;
- only allowlisted bounded attributes and approved span-only identifiers are captured;
- span-only identifiers are absent from stderr, OTLP logs, and Prometheus output;
- forbidden secrets, headers, payload fragments, commands, actors, raw reasons, and raw URLs are
  absent from every capture;
- existing response, persistence, and metric tests remain unchanged and passing.

The implementation is complete only after `just fmt`, `cargo build`,
`cargo clippy --all-targets -- -D warnings`, `just test`, and `cargo doc --no-deps` pass.

## Scope boundary

This child does not add completed workflow job or step traces, telemetry failure/drop metrics,
rate-limited exporter diagnostics, or final process-level provider shutdown integration. Those
remain assigned to later Phase 4 children.
