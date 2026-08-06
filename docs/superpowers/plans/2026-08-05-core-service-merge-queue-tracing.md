# Core Service and Merge-Queue Tracing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Export coherent, bounded, privacy-preserving traces for core HTTP, webhook, repository, SQLite, merge-queue, and retention operations without changing application behavior.

**Architecture:** A new `telemetry::trace` policy module owns stable operation names, attribute keys, bounded values, span-only identifier helpers, status, and events. Axum middleware creates request roots; handlers, stores, and retention create explicit children with `tracing::Instrument`, while `tracing-opentelemetry::OpenTelemetrySpanExt` bypasses ordinary tracing fields for diagnostic identifiers.

**Tech Stack:** Rust 2021, Axum 0.8, Tokio, tracing 0.1, tracing-opentelemetry 0.33, OpenTelemetry 0.32, SQLx SQLite, OTLP/HTTP protobuf test receiver.

## Global Constraints

- Preserve all existing HTTP bodies/statuses, persistence transitions, Prometheus values, deduplication behavior, and redaction guarantees.
- Stable operations are exactly `http.request`, `github.webhook.authenticate`, `github.webhook.process`, `config.repository.write`, `sqlite.query`, and `merge_queue.update`.
- Diagnostic identifiers may be attached only through `OpenTelemetrySpanExt::set_attribute`, never ordinary `tracing` fields or events.
- Allowed core identifiers are canonical repository name, repository ID, pull-request number, delivery ID, and relevant SHA.
- Never attach request bodies, payload fragments, secrets, signatures, authorization/OTLP headers, actors, raw URLs, commands, raw actions, raw reasons, error strings, or SQL statements.
- Specialized merge-group and pull-request transition spans/events occur only after a new delivery claim.
- Group `merged` remains authoritative; group `dequeued` stays normalized; PR dequeue remains `unknown/unclassified_dequeue`.
- Queue-state failures set bounded span event/status but preserve authenticated `204`.
- Retention operations start independent trace roots.
- Every new public item requires doc comments; production code must not use `unwrap()`.

---

## File Structure

- Create `src/telemetry/trace.rs`: central span names, keys, bounded enums, constructors, identifier setters, status, and bounded events.
- Modify `src/telemetry.rs`: register the focused trace policy module and expand private OTLP test utilities.
- Modify `src/app.rs`: apply all-route request instrumentation at the outer router boundary.
- Modify `src/api/repositories.rs`: add repository write spans and approved identifiers.
- Modify `src/api/webhook.rs`: add authentication/process spans, normalized event attributes, and span-only repository/delivery identifiers.
- Modify `src/api/merge_group.rs`: expose normalized merge-group processing data and create specialized group update spans.
- Modify `src/api/pull_request.rs`: project core PR/SHA fields and create specialized queue update spans/status/events.
- Modify `src/metrics.rs`: expose existing bounded enum string encodings within the crate rather than duplicating mappings.
- Modify `src/storage/repository_store.rs`: add named SQLite spans around repository operations.
- Modify `src/storage/delivery_store.rs`: add named SQLite spans around claim/prune operations.
- Modify `src/storage/merge_queue_store.rs`: add named SQLite spans around queue operations.
- Modify `src/retention.rs`: create independent maintenance roots and bounded outcomes.
- Modify `src/telemetry/otlp_test.rs`: reusable protobuf capture assertions and end-to-end hierarchy/privacy regressions.
- Modify `docs/operations.md`: document core trace names and span-only identifier policy.
- Create `changelog/2026-08-05T19-39-20-0400-core-service-merge-queue-tracing.md`: record the implementation and validation.

### Task 1: Central trace policy and span-only identifiers

**Files:**
- Create: `src/telemetry/trace.rs`
- Modify: `src/telemetry.rs:1-6`
- Modify: `src/metrics.rs:110-137, 206-248, 329-433`
- Test: `src/telemetry/trace.rs`

**Interfaces:**
- Consumes: `metrics::{Action, EventType, MergeGroupReason, MergeQueueOutcome, MergeQueueReason}` and typed domain identifiers.
- Produces: `operation_span`, `database_span`, `set_repository_name`, `set_repository_id`, `set_delivery_id`, `set_pull_request_number`, `set_commit_sha`, `set_status`, `add_failure_event`, `Operation`, `DatabaseOperation`, and `OperationOutcome` for all later tasks.

- [ ] **Step 1: Write failing unit tests for bounded values and span-only behavior**

Add `#[cfg(test)]` tests in the new module that create a tracing span under a registry with both fmt and OpenTelemetry layers. Capture fmt output and in-memory span data, then assert:

```rust
assert_eq!(DatabaseOperation::RepositoryCreate.as_str(), "repository.create");
assert_eq!(DatabaseOperation::DeliveryClaim.as_str(), "delivery.claim");
assert_eq!(OperationOutcome::Failure.as_str(), "failure");
assert!(exported.attributes.contains_key("github.repository.name"));
assert!(exported.attributes.contains_key("github.delivery.id"));
assert!(!stderr.contains("owner/private-repository"));
assert!(!stderr.contains(TEST_DELIVERY_ID));
```

Also enumerate every enum variant in table-driven assertions so no catch-all can silently widen values.

- [ ] **Step 2: Run the focused test and verify it fails**

Run: `cargo test telemetry::trace::tests --lib`

Expected: FAIL because `telemetry::trace`, `DatabaseOperation`, and identifier helpers do not exist.

- [ ] **Step 3: Implement the central policy module**

Create `src/telemetry/trace.rs` with fixed constants and explicit enums. Use this API shape:

```rust
use opentelemetry::{trace::Status, Key, KeyValue, Value};
use tracing::{info_span, Span};
use tracing_opentelemetry::OpenTelemetrySpanExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Operation {
    HttpRequest,
    WebhookAuthenticate,
    WebhookProcess,
    RepositoryWrite,
    SqliteQuery,
    MergeQueueUpdate,
    RetentionRun,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OperationOutcome {
    Success,
    Duplicate,
    NoOp,
    Cancelled,
    Failure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DatabaseOperation {
    RepositoryCount,
    RepositoryCreate,
    RepositoryList,
    RepositoryAuthenticate,
    RepositoryGet,
    RepositoryUpdate,
    RepositoryDelete,
    DeliveryClaim,
    DeliveryPrune,
    MergeQueueEnqueue,
    MergeQueueComplete,
    MergeQueuePrune,
}

pub(crate) fn operation_span(operation: Operation) -> Span {
    match operation {
        Operation::HttpRequest => info_span!(target: "github_webhook_exporter", "http.request"),
        Operation::WebhookAuthenticate => {
            info_span!(target: "github_webhook_exporter", "github.webhook.authenticate")
        }
        Operation::WebhookProcess => {
            info_span!(target: "github_webhook_exporter", "github.webhook.process")
        }
        Operation::RepositoryWrite => {
            info_span!(target: "github_webhook_exporter", "config.repository.write")
        }
        Operation::SqliteQuery => info_span!(target: "github_webhook_exporter", "sqlite.query"),
        Operation::MergeQueueUpdate => {
            info_span!(target: "github_webhook_exporter", "merge_queue.update")
        }
        Operation::RetentionRun => {
            info_span!(target: "github_webhook_exporter", parent: None, "retention.run")
        }
    }
}

pub(crate) fn database_span(operation: DatabaseOperation) -> Span {
    let span = operation_span(Operation::SqliteQuery);
    span.set_attribute("db.system.name", "sqlite");
    span.set_attribute("db.operation.name", operation.as_str());
    span
}

pub(crate) fn set_status(span: &Span, outcome: OperationOutcome) {
    span.set_attribute("ghe.operation.outcome", outcome.as_str());
    span.set_status(match outcome {
        OperationOutcome::Failure => Status::error("operation_failed"),
        OperationOutcome::Success
        | OperationOutcome::Duplicate
        | OperationOutcome::NoOp
        | OperationOutcome::Cancelled => Status::Ok,
    });
}

pub(crate) fn add_failure_event(span: &Span, reason: &'static str) {
    span.add_event(
        "operation.failure",
        vec![KeyValue::new("ghe.failure.reason", reason)],
    );
}
```

Implement all `as_str` methods with exhaustive matches. Add typed setter functions taking `&Span` and typed/validated inputs. Encode `DeliveryId` into a stack UUID buffer and convert only at the final OTel boundary. Do not accept arbitrary keys from callers:

```rust
pub(crate) fn set_repository_name(span: &Span, name: &CanonicalRepositoryName) {
    span.set_attribute("github.repository.name", name.as_str().to_owned());
}

pub(crate) fn set_repository_id(span: &Span, id: RepositoryId) {
    span.set_attribute("github.repository.id", id.get());
}

pub(crate) fn set_pull_request_number(span: &Span, number: PullRequestNumber) {
    span.set_attribute("github.pull_request.number", number.get());
}
```

Register it with `mod trace;` in `src/telemetry.rs`. Change only existing bounded enum `as_str` methods needed by telemetry from private to `pub(crate)`.

- [ ] **Step 4: Run focused tests and lint the module**

Run: `cargo test telemetry::trace::tests --lib && cargo clippy --lib -- -D warnings`

Expected: PASS with identifier values present in exported span data and absent from fmt output.

- [ ] **Step 5: Commit the policy boundary**

```bash
git add src/telemetry.rs src/telemetry/trace.rs src/metrics.rs
git commit -m "feat: centralize bounded trace policy"
```

### Task 2: HTTP roots and repository API hierarchy

**Files:**
- Modify: `src/app.rs:7-17, 94-103`
- Modify: `src/api/repositories.rs:1-15, 90-192`
- Test: `src/telemetry/otlp_test.rs`
- Test: `tests/repository_api.rs`

**Interfaces:**
- Consumes: `trace::operation_span`, `trace::set_status`, route/status/result attribute helpers, and span-only repository setters from Task 1.
- Produces: one `http.request` root for every matched route and `config.repository.write` children for create/update/delete.

- [ ] **Step 1: Add failing OTLP hierarchy tests for repository routes**

Extend `src/telemetry/otlp_test.rs` with a receiver fixture that can return flattened spans after `runtime.force_flush()`. Exercise authenticated create, list, update, get, and delete requests through `build_router`. Assert fixed names and parent IDs:

```rust
let request = captured.span_named("http.request");
assert_eq!(request.string_attribute("http.request.method"), Some("POST"));
assert_eq!(
    request.string_attribute("http.route"),
    Some("/api/v1/repositories")
);
assert_eq!(request.i64_attribute("http.response.status_code"), Some(201));
let write = captured.child_named(request, "config.repository.write");
assert_eq!(write.string_attribute("ghe.config.operation"), Some("create"));
assert_eq!(write.string_attribute("github.repository.name"), Some("owner/repository"));
```

Add malformed JSON, unauthorized, unknown route, and store-failure cases. Assert raw URI query strings and authorization values are absent from serialized trace requests.

- [ ] **Step 2: Run the repository trace test and verify it fails**

Run: `cargo test telemetry::otlp_test::repository --lib -- --nocapture`

Expected: FAIL because no request or repository-write spans are emitted.

- [ ] **Step 3: Add all-route request middleware**

In `src/app.rs`, add an outer middleware after router composition. Read `MatchedPath` before consuming the request; never inspect `request.uri()`. Instrument `next.run(request)` so all descendants inherit the request span:

```rust
async fn observe_http_request(request: Request, next: Next) -> Response {
    let method = request.method().clone();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or("unmatched", MatchedPath::as_str);
    let span = trace::operation_span(Operation::HttpRequest);
    trace::set_http_method(&span, &method);
    trace::set_route(&span, route);
    let response = next.run(request).instrument(span.clone()).await;
    trace::set_http_response(&span, response.status());
    response
}
```

Use a bounded method mapper (`GET`, `POST`, `PATCH`, `DELETE`, `HEAD`, `OPTIONS`, `PUT`, `other`) and bounded result mapper (`success`, `client_error`, `server_error`, `other`). Preserve the existing webhook metrics middleware.

- [ ] **Step 4: Add explicit repository write children**

Wrap create/update/delete store futures in `config.repository.write` spans. Set only operation (`create|update|delete`) as a bounded attribute. Attach repository ID before update/delete; attach returned ID and canonical name after successful create/update through span-only setters. Mark success/failure from typed results without adding error text.

Use `Future::instrument` rather than entering a span across `.await`:

```rust
let write_span = trace::operation_span(Operation::RepositoryWrite);
trace::set_config_operation(&write_span, ConfigOperation::Create);
trace::set_repository_name(&write_span, &full_name);
let result = state
    .repository_store()
    .create(full_name, webhook_secret, request.enabled)
    .instrument(write_span.clone())
    .await;
trace::set_result_status(&write_span, &result);
let metadata = result.map_err(AppError::repository_store)?;
trace::set_repository_id(&write_span, metadata.id());
```

- [ ] **Step 5: Run trace and existing repository regressions**

Run: `cargo test telemetry::otlp_test::repository --lib && cargo test --test repository_api`

Expected: PASS; existing response and redaction assertions remain unchanged.

- [ ] **Step 6: Commit request and repository instrumentation**

```bash
git add src/app.rs src/api/repositories.rs src/telemetry/otlp_test.rs tests/repository_api.rs
git commit -m "feat: trace HTTP and repository operations"
```

### Task 3: Webhook authentication, processing, and core identifiers

**Files:**
- Modify: `src/api/webhook.rs:1-185`
- Modify: `src/api/merge_group.rs:1-75`
- Modify: `src/api/pull_request.rs:1-145`
- Test: `src/telemetry/otlp_test.rs`
- Test: `tests/webhook_api.rs`

**Interfaces:**
- Consumes: request roots and trace policy from Tasks 1-2.
- Produces: `github.webhook.authenticate`, `github.webhook.process`, and newly-claimed `merge_queue.update` spans with repository, delivery, PR, and SHA identifiers.

- [ ] **Step 1: Add failing webhook hierarchy and semantic tests**

Build table-driven OTLP cases for accepted merge-group, PR enqueue, PR dequeue, unauthorized, malformed, delivery-store unavailable, queue-store failure, and duplicate delivery. Assert:

```rust
let request = spans.one_named("http.request");
let authenticate = spans.child_named(request, "github.webhook.authenticate");
let process = spans.child_named(request, "github.webhook.process");
let claim = spans.descendant_named(process, "sqlite.query", "delivery.claim");
let update = spans.descendant_named(process, "merge_queue.update", "pull_request");
assert_eq!(update.string_attribute("ghe.queue.outcome"), Some("unknown"));
assert_eq!(
    update.string_attribute("ghe.queue.reason"),
    Some("unclassified_dequeue")
);
```

Send the same delivery twice and assert exactly one specialized `merge_queue.update` span. For queue failure, assert response `204`, event name `operation.failure`, bounded reason `queue_state`, and error status. Assert repository name/ID, delivery ID, PR number, and SHA are span attributes but absent from captured logs, stderr, and Prometheus text.

- [ ] **Step 2: Run webhook trace tests and verify they fail**

Run: `cargo test telemetry::otlp_test::webhook --lib -- --nocapture`

Expected: FAIL because webhook child spans and diagnostic attributes do not exist.

- [ ] **Step 3: Instrument authentication and process boundaries**

Create authentication and process spans in `webhook_handler`. Add repository name and delivery ID to both via span-only helpers. Instrument authentication, then set repository ID only after success. Put JSON projection, claim, generic metrics, and specialized work inside the process span. Record normalized event/action only after parsing, using existing bounded enums.

Use explicit result branches so status is set before each return:

```rust
let authentication_span = trace::operation_span(Operation::WebhookAuthenticate);
trace::set_repository_name(&authentication_span, &request.repository_name);
trace::set_delivery_id(&authentication_span, &request.delivery_id);
let authentication = WebhookAuthenticator::new(state.repository_store())
    .authenticate(&request.repository_name, &request.signature, request.body.as_ref())
    .instrument(authentication_span.clone())
    .await;
```

Do not add body, signature, raw header event type, or raw action fields.

- [ ] **Step 4: Project and attach core PR/SHA identifiers safely**

Extend only the authenticated specialized projections:

```rust
#[derive(Deserialize)]
struct HeadProjection {
    sha: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct PullRequestProjection {
    number: i64,
    updated_at: Option<String>,
    merged: Option<bool>,
    head: Option<HeadProjection>,
}
```

For merge-group payloads, project `merge_group.head_sha`. Validate SHA values before attachment: exactly 40 or 64 ASCII hexadecimal characters, normalized to lowercase, otherwise omit. Introduce a private validated `CommitSha` newtype in `telemetry::trace` or the API projection module; never retain malformed values in errors/logs.

- [ ] **Step 5: Emit specialized spans only after `DeliveryClaim::New`**

Refactor `EventProjection::process_merge_group` to return an optional normalized transition descriptor rather than recording metrics invisibly:

```rust
pub(super) struct MergeGroupTransition {
    pub(super) action: MergeGroupAction,
    pub(super) reason: MergeGroupReason,
    pub(super) head_sha: Option<CommitSha>,
}
```

The webhook handler records the existing metric and emits `merge_queue.update` for the descriptor only in the `New` branch. `QueueProcessor::process` similarly creates a span only after event type/action/PR validation establishes a supported transition. Attach normalized queue outcome/reason and typed IDs. On store error call:

```rust
trace::add_failure_event(&update_span, "queue_state");
trace::set_status(&update_span, OperationOutcome::Failure);
```

Return the same result as before so `record_queue_state_failure` still preserves `204`.

- [ ] **Step 6: Run webhook trace and behavior regressions**

Run: `cargo test telemetry::otlp_test::webhook --lib && cargo test --test webhook_api`

Expected: PASS, including duplicate suppression, dequeue semantics, queue rollback behavior, status codes, metrics, and redaction.

- [ ] **Step 7: Commit webhook instrumentation**

```bash
git add src/api/webhook.rs src/api/merge_group.rs src/api/pull_request.rs src/telemetry/trace.rs src/telemetry/otlp_test.rs tests/webhook_api.rs
git commit -m "feat: trace webhook and merge-queue processing"
```

### Task 4: Named SQLite child spans

**Files:**
- Modify: `src/storage/repository_store.rs:1-385`
- Modify: `src/storage/delivery_store.rs:1-127`
- Modify: `src/storage/merge_queue_store.rs:1-212`
- Test: `src/telemetry/otlp_test.rs`
- Test: `tests/storage.rs`
- Test: `tests/delivery_storage.rs`
- Test: `tests/merge_queue_storage.rs`

**Interfaces:**
- Consumes: `trace::database_span(DatabaseOperation)` and status helpers from Task 1.
- Produces: one `sqlite.query` span around each logical public store operation without exposing SQL.

- [ ] **Step 1: Add failing SQLite operation-name tests**

In OTLP tests, exercise repository create/read/write, authentication lookup, delivery claim, queue enqueue/complete, and prune. Assert each span contains only:

```rust
assert_eq!(span.name(), "sqlite.query");
assert_eq!(span.string_attribute("db.system.name"), Some("sqlite"));
assert_eq!(span.string_attribute("db.operation.name"), Some("merge_queue.complete"));
assert!(!span.attribute_keys().any(|key| key.contains("statement")));
```

Assert SQLite errors set error status with `ghe.operation.outcome=failure` but contain no database message, table name from an injected trigger, path, or SQL statement.

- [ ] **Step 2: Run SQLite trace tests and verify they fail**

Run: `cargo test telemetry::otlp_test::sqlite --lib -- --nocapture`

Expected: FAIL because store operations are not instrumented.

- [ ] **Step 3: Wrap each store method once at its logical boundary**

For each public async method, move existing implementation into a private `*_inner` method or an inline async block, instrument it with the matching `DatabaseOperation`, and set status from the typed result:

```rust
pub async fn claim(
    &self,
    delivery_id: &DeliveryId,
) -> Result<DeliveryClaim, DeliveryStoreError> {
    let span = trace::database_span(DatabaseOperation::DeliveryClaim);
    let result = self.claim_inner(delivery_id).instrument(span.clone()).await;
    trace::set_result_status(&span, &result);
    result
}
```

Use exactly one span per logical store call even when a transaction executes multiple SQL statements. Never add SQL, bind values, database path, row data, error display/debug, or table names.

- [ ] **Step 4: Run all storage and OTLP SQLite tests**

Run: `cargo test telemetry::otlp_test::sqlite --lib && cargo test --test storage && cargo test --test delivery_storage && cargo test --test merge_queue_storage`

Expected: PASS with durable state and error mapping unchanged.

- [ ] **Step 5: Commit storage instrumentation**

```bash
git add src/storage/repository_store.rs src/storage/delivery_store.rs src/storage/merge_queue_store.rs src/telemetry/otlp_test.rs
git commit -m "feat: trace bounded SQLite operations"
```

### Task 5: Independent retention roots

**Files:**
- Modify: `src/retention.rs:1-229`
- Test: `src/retention.rs`
- Test: `src/telemetry/otlp_test.rs`

**Interfaces:**
- Consumes: `operation_span(Operation::RetentionRun)`, bounded workload/outcome setters, and store SQLite spans.
- Produces: one independent `retention.run` root per scheduled pass with delivery and merge-queue SQLite descendants.

- [ ] **Step 1: Add failing retention-root tests**

Use paused Tokio time to trigger a pass under an unrelated ambient request span. Flush captures and assert:

```rust
let retention = spans.one_named("retention.run");
assert!(retention.parent_span_id().is_invalid());
assert!(spans.children(retention).any(|span| {
    span.name() == "sqlite.query"
        && span.string_attribute("db.operation.name") == Some("delivery.prune")
}));
assert!(spans.children(retention).any(|span| {
    span.name() == "sqlite.query"
        && span.string_attribute("db.operation.name") == Some("merge_queue.prune")
}));
```

Cover completed, cancelled-between-workloads, invalid-cutoff, and one-store-failure cases with bounded outcomes only.

- [ ] **Step 2: Run retention trace tests and verify they fail**

Run: `cargo test telemetry::otlp_test::retention --lib -- --nocapture`

Expected: FAIL because retention does not create root spans.

- [ ] **Step 3: Instrument each scheduled pass as an independent root**

At each ticker event, create `operation_span(Operation::RetentionRun)`, instrument `prune_retention_pass`, and set the final bounded result. Keep workload-specific outcomes on the corresponding SQLite spans or bounded OTel events. Do not attach cutoff timestamps, counts as identifiers, errors, or correlation IDs to traces.

```rust
let retention_span = trace::operation_span(Operation::RetentionRun);
prune_retention_pass(
    &delivery_store,
    &merge_queue_store,
    config,
    &shutdown,
)
.instrument(retention_span.clone())
.await;
```

Return a small internal `RetentionPassOutcome` from `prune_retention_pass` so the caller can set `success|cancelled|failure` without parsing logs. Preserve existing logs and loop behavior.

- [ ] **Step 4: Run retention tests**

Run: `cargo test retention --lib && cargo test telemetry::otlp_test::retention --lib`

Expected: PASS; cancellation, batching, error isolation, and structured log assertions remain unchanged.

- [ ] **Step 5: Commit maintenance tracing**

```bash
git add src/retention.rs src/telemetry/otlp_test.rs
git commit -m "feat: trace retention as independent roots"
```

### Task 6: Integrated privacy matrix, documentation, and project gates

**Files:**
- Modify: `src/telemetry/otlp_test.rs`
- Modify: `docs/operations.md`
- Create: `changelog/2026-08-05T19-39-20-0400-core-service-merge-queue-tracing.md`

**Interfaces:**
- Consumes: all instrumentation from Tasks 1-5.
- Produces: complete issue #34 acceptance evidence and operator documentation.

- [ ] **Step 1: Add the integrated forbidden-value and behavior matrix**

Drive repository API, webhook success/failure/duplicate, merge-group dequeued, PR dequeue, and retention through one in-process OTLP receiver. Include unique sentinel values for every forbidden class:

```rust
const SECRET: &str = "forbidden-webhook-secret";
const SIGNATURE: &str =
    "sha256=0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
const AUTHORIZATION: &str = "Bearer forbidden-admin-token";
const ACTOR: &str = "forbidden-actor";
const COMMAND: &str = "forbidden-command";
const RAW_REASON: &str = "forbidden-raw-reason";
const RAW_URL: &str = "https://forbidden.invalid/private";
```

Serialize captured trace protobuf, log protobuf, stderr, and Prometheus exposition. Assert all forbidden sentinels are absent. Separately assert approved identifiers are present in traces and absent from the other three outputs. Assert all attribute keys belong to the explicit resource/span/event allowlist.

- [ ] **Step 2: Run the integrated test and correct only instrumentation defects**

Run: `cargo test telemetry::otlp_test::integrated_core_trace_privacy --lib -- --nocapture`

Expected: PASS. If it fails, change instrumentation or the allowlist; do not weaken sentinel assertions unless the issue contract explicitly permits the value.

- [ ] **Step 3: Document operations and the implementation iteration**

Add an “Exported core traces” section to `docs/operations.md` listing the six stable issue operations plus independent `retention.run`, route-template behavior, bounded attributes, span-only core identifiers, and explicit forbidden classes. State that workflow run/attempt/job identifiers are deferred to #10.

Create a timestamped changelog entry recording:

- central policy module;
- request/repository/webhook/SQLite/merge-queue/retention hierarchy;
- duplicate and failure semantics;
- span-only identifiers and privacy coverage;
- exact validation commands and results.

- [ ] **Step 4: Run the mandatory validation sequence from the top**

Run in order:

```bash
just fmt
cargo build
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps
```

Expected: every command exits 0 with no warnings. If any command fails, fix the defect and rerun the entire sequence from `just fmt`.

- [ ] **Step 5: Inspect the final diff for policy violations**

Run:

```bash
git diff --check
git diff --stat origin/main...HEAD
rg -n 'request\.uri|http\.url|db\.statement|authorization|signature|request_body|payload' src/telemetry src/api src/storage src/retention.rs
```

Expected: no whitespace errors; every search hit is a test sentinel, existing business input, or explicit prohibition—not an emitted trace field.

- [ ] **Step 6: Commit integrated tests and documentation**

```bash
git add src/telemetry/otlp_test.rs docs/operations.md changelog/
git commit -m "test: verify core trace privacy and hierarchy"
```

- [ ] **Step 7: Confirm issue #10’s handoff remains explicit**

Run:

```bash
gh issue view 10 --json body -q .body | rg 'Identifier boundary with #34|workflow run ID|workflow job ID|shared span-only'
```

Expected: all expressions match the updated issue body.
