# Repository-Scoped Observability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Distinguish authenticated GitHub repositories in every repository-scoped Prometheus metric and in the root webhook HTTP trace using canonical `owner/repository` identity.

**Architecture:** Install a shared request-local repository context in the outer HTTP middleware and set it only after webhook authentication succeeds. Extend repository-scoped Prometheus family labels with a typed repository value; direct post-authentication call sites pass the canonical name, while request middleware maps an unset context to the fixed `unknown` value. Reuse the context to annotate the completed root `http.request` span.

**Tech Stack:** Rust 2021, Axum request extensions and middleware, `prometheus-client`, `tracing`, OpenTelemetry, Tokio, existing canonical repository domain type.

## Global Constraints

- Prometheus label name is exactly `repository`.
- Authenticated values are canonical lowercase full names in `owner/repository` form.
- Pre-authentication failures use exactly `repository="unknown"`.
- Never publish an unauthenticated payload repository as telemetry identity.
- Do not add repository labels to `github_repository_configurations` or OTLP diagnostic metrics.
- Add no dependencies, environment variables, or configuration flags.
- Preserve existing redaction and bounded-vocabulary guarantees.

---

### Task 1: Typed repository metric labels

**Files:**
- Modify: `src/metrics.rs`
- Modify: `src/security/mod.rs`

**Interfaces:**
- Consumes: `CanonicalRepositoryName::as_str(&self) -> &str`.
- Produces: repository-aware `Metrics` recording methods accepting either
  `&CanonicalRepositoryName` or `Option<&CanonicalRepositoryName>` for request outcomes.

- [ ] **Step 1: Write failing metric tests**

Add a fixture helper and assertions in `src/metrics.rs` proving two canonical repositories remain
separate and process-wide metrics remain unlabeled:

```rust
fn repository_name(value: &str) -> CanonicalRepositoryName {
    CanonicalRepositoryName::new(value).expect("repository name is valid")
}

#[test]
fn repository_scoped_metrics_use_full_canonical_names() {
    let metrics = Metrics::new();
    let first = repository_name("PeterGrace/GitHub-Webhook-Exporter");
    let second = repository_name("Other/Repository");

    metrics.observe_event(&first, EventType::Push, Action::None, 128);
    metrics.observe_event(&second, EventType::Push, Action::None, 256);

    let exposition = metrics.encode().expect("metrics encode");
    assert!(exposition.contains(
        "github_webhook_events_total{repository=\"petergrace/github-webhook-exporter\",event_type=\"push\",action=\"none\"} 1"
    ));
    assert!(exposition.contains(
        "github_webhook_events_total{repository=\"other/repository\",event_type=\"push\",action=\"none\"} 1"
    ));
}
```

Extend the test across request, body-size, duplicate, failure, merge-group, merge-queue, workflow
step, and workflow rejection families. Assert `github_repository_configurations` and
`github_telemetry_*` samples do not contain `repository=`.

- [ ] **Step 2: Run the focused test to verify it fails**

Run:

```bash
cargo test metrics::tests::repository_scoped_metrics_use_full_canonical_names --lib -- --nocapture
```

Expected: compilation fails because `Metrics::observe_event` does not accept a repository.

- [ ] **Step 3: Implement the typed label**

Derive `Hash` for `CanonicalRepositoryName`. Add a private metric label that makes the fallback
explicit and cannot contain arbitrary invalid text:

```rust
const UNKNOWN_REPOSITORY: &str = "unknown";

#[derive(Clone, Debug, Hash, PartialEq, Eq)]
enum RepositoryLabel {
    Authenticated(CanonicalRepositoryName),
    Unknown,
}

impl RepositoryLabel {
    fn authenticated(repository: &CanonicalRepositoryName) -> Self {
        Self::Authenticated(repository.clone())
    }

    fn optional(repository: Option<&CanonicalRepositoryName>) -> Self {
        repository.map_or(Self::Unknown, Self::authenticated)
    }
}

impl EncodeLabelValue for RepositoryLabel {
    fn encode(&self, encoder: &mut LabelValueEncoder) -> fmt::Result {
        encoder.write_str(match self {
            Self::Authenticated(repository) => repository.as_str(),
            Self::Unknown => UNKNOWN_REPOSITORY,
        })
    }
}
```

Add `repository: RepositoryLabel` to each repository-scoped label set. Convert standalone
repository-scoped counters and histograms into `Family` values with a repository-only label set.
Seed only `RepositoryLabel::Unknown`, and update every repository-scoped recording method to require
a canonical repository. Let `observe_request` alone accept `Option<&CanonicalRepositoryName>`.
Ensure invalid merge-queue durations carry the same repository into the transition-failure metric.

- [ ] **Step 4: Update existing unit call sites and run metric tests**

Use one canonical fixture throughout each existing test and update expected exposition strings with
the `repository` label. Run:

```bash
cargo test metrics::tests --lib -- --nocapture
```

Expected: all metrics unit tests pass.

- [ ] **Step 5: Commit the typed metrics change**

```bash
git add src/metrics.rs src/security/mod.rs
git commit -m "feat: scope webhook metrics by repository"
```

### Task 2: Authenticated request repository context

**Files:**
- Modify: `src/app.rs`
- Modify: `src/api/webhook.rs`
- Modify: `src/api/pull_request.rs`
- Test: `tests/webhook_api.rs`
- Test: `src/telemetry/otlp_test.rs`

**Interfaces:**
- Consumes: repository-aware metric methods from Task 1 and
  `trace::set_repository_name(&Span, &CanonicalRepositoryName)`.
- Produces: `RequestRepositoryContext` installed for every HTTP request and set once after webhook
  authentication.

- [ ] **Step 1: Write failing two-repository API tests**

In `tests/webhook_api.rs`, configure `PeterGrace/GitHub-Webhook-Exporter` and
`Other/Repository`, submit one valid uniquely identified webhook to each, and assert distinct full
canonical values:

```rust
assert!(exposition.contains(
    "repository=\"petergrace/github-webhook-exporter\""
));
assert!(exposition.contains("repository=\"other/repository\""));
assert!(!exposition.contains("repository=\"github-webhook-exporter\""));
```

Add an unauthorized payload with `attacker/arbitrary` and assert it creates only an `unknown`
request series and never exposes `attacker/arbitrary`.

- [ ] **Step 2: Add a failing OTLP root-span assertion**

Extend the in-process OTLP webhook test in `src/telemetry/otlp_test.rs` to assert the webhook
`http.request` root has:

```rust
assert_eq!(
    request.string_attribute("github.repository.name"),
    Some("petergrace/github-webhook-exporter")
);
```

Also retain existing assertions on authentication, process, merge-queue, and workflow spans.

- [ ] **Step 3: Run focused API and OTLP tests to verify failure**

Run:

```bash
cargo test --test webhook_api repository_scoped_metrics_distinguish_full_names -- --nocapture
cargo test telemetry::otlp_test --lib -- --nocapture
```

Expected: metric signatures/call sites or root-span repository assertions fail.

- [ ] **Step 4: Add and propagate the request context**

In `src/app.rs`, add a crate-visible cloneable context backed by
`Arc<OnceLock<CanonicalRepositoryName>>`:

```rust
#[derive(Clone, Debug, Default)]
pub(crate) struct RequestRepositoryContext(Arc<OnceLock<CanonicalRepositoryName>>);

impl RequestRepositoryContext {
    pub(crate) fn authenticate(&self, repository: &CanonicalRepositoryName) {
        let inserted = self.0.set(repository.clone()).is_ok();
        debug_assert!(inserted, "repository context is authenticated once");
    }

    pub(crate) fn repository(&self) -> Option<&CanonicalRepositoryName> {
        self.0.get()
    }
}
```

Create and insert the context in `observe_http_request` before calling the next service. After the
response, annotate the root span when `repository()` is present.

Read the installed context in `WebhookRequest::from_request`. After successful authentication,
call `context.authenticate(&request.repository_name)`. The webhook metrics middleware reads the
same context after `next.run` and passes `context.repository()` to `observe_request`.

Pass `&request.repository_name` into every direct repository-scoped metric call. Extend
`QueueProcessor` metric calls to pass `self.repository_name`, including invalid-duration and
missing-active-attempt paths. Pass repository identity into failure helpers so post-authentication
failures are repository-scoped.

- [ ] **Step 5: Run focused integration and trace tests**

Run:

```bash
cargo test --test webhook_api -- --nocapture
cargo test telemetry::otlp_test --lib -- --nocapture
```

Expected: all webhook metric, redaction, and trace tests pass.

- [ ] **Step 6: Commit request propagation**

```bash
git add src/app.rs src/api/webhook.rs src/api/pull_request.rs \
    tests/webhook_api.rs src/telemetry/otlp_test.rs
git commit -m "feat: propagate authenticated repository observability"
```

### Task 3: Documentation and complete validation

**Files:**
- Modify: `docs/operations.md`
- Modify: `docs/superpowers/specs/2026-08-03-02-webhook-ingestion-and-metrics.md`
- Modify: `docs/superpowers/specs/2026-08-03-03-merge-queue-tracking.md`
- Modify: `docs/superpowers/specs/2026-08-05-core-service-merge-queue-tracing-design.md`
- Modify: `docs/superpowers/specs/2026-08-06-workflow-job-otlp-traces-design.md`
- Create: `changelog/2026-08-10T15-45-01Z-repository-scoped-observability.md`

**Interfaces:**
- Consumes: completed metric and trace behavior from Tasks 1-2.
- Produces: operator-facing label semantics and a timestamped implementation record.

- [ ] **Step 1: Update documentation**

Document the exact repository-scoped metric list, canonical lowercase `owner/repository` format,
fixed `unknown` fallback, authenticated-only trust boundary, process-wide exclusions, and root span
attribute behavior. Replace stale statements that repository names never appear as metric labels.

- [ ] **Step 2: Add the timestamped changelog**

Record issue #64, implementation behavior, security/cardinality decisions, tests, and documentation
changes under `changelog/` using the current UTC date and time.

- [ ] **Step 3: Run formatting and compile gates**

```bash
cargo fmt --all
cargo fmt --all -- --check
cargo build
cargo clippy --all-targets -- -D warnings
```

Expected: every command exits successfully without warnings.

- [ ] **Step 4: Run the full test gate**

```bash
cargo test --all-targets
```

Expected: all library, binary, and integration tests pass.

- [ ] **Step 5: Inspect the final diff and commit documentation**

```bash
git diff --check
git status --short
git add docs/operations.md \
    docs/superpowers/specs/2026-08-03-02-webhook-ingestion-and-metrics.md \
    docs/superpowers/specs/2026-08-03-03-merge-queue-tracking.md \
    docs/superpowers/specs/2026-08-05-core-service-merge-queue-tracing-design.md \
    docs/superpowers/specs/2026-08-06-workflow-job-otlp-traces-design.md \
    changelog/2026-08-10T15-45-01Z-repository-scoped-observability.md
git commit -m "docs: describe repository-scoped observability"
```

- [ ] **Step 6: Link implementation to the issue**

Add a GitHub issue comment summarizing the implementation commits and validation evidence. Do not
close issue #64 until the implementation is merged.
