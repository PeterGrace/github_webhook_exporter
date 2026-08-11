# OTLP HTTPS and Failure Diagnostics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Enable HTTPS for the existing blocking OTLP exporters and emit one safe, more specific stderr diagnostic for every failed export.

**Architecture:** Keep the existing OpenTelemetry HTTP exporter and `ObservingHttpClient`, but compile `reqwest` with its rustls backend. Pass a small closed failure-context type from typed `reqwest::Error` predicates to the direct diagnostics sink; never render raw dependency errors or collector-controlled values.

**Tech Stack:** Rust 2021, reqwest 0.13 blocking client with rustls, OpenTelemetry 0.32 OTLP/HTTP protobuf, Prometheus client, Cargo.

## Global Constraints

- Keep `reqwest` default features disabled and add only the existing `blocking` plus `rustls` features.
- Do not replace the OpenTelemetry HTTP backend or introduce a second HTTP client.
- Emit every export failure directly to stderr; do not send diagnostics through `tracing` or OTLP logs.
- Keep queue-drop diagnostics rate-limited to one line per signal/reason per monotonic minute.
- Never log endpoint URLs, headers, credentials, request payloads, response bodies, raw errors, or error source chains.
- Failure fields are closed to `status=<u16>` and `detail=connect|request_builder|redirect|request`.
- Preserve existing Prometheus metric names, labels, and exactly-once accounting per failed attempt.
- Collector availability must not affect webhook responses or readiness.
- Follow test-driven development: run each new test red before changing production code.

## File structure

- `Cargo.toml`, `Cargo.lock`: compile the existing blocking reqwest client with rustls.
- `src/telemetry.rs`: construct the production HTTP client and host the local HTTPS capability regression.
- `src/telemetry/diagnostics.rs`: own bounded diagnostic context, exact direct-stderr rendering, and drop-only limiting.
- `src/telemetry/http_client.rs`: classify typed reqwest failures into reason plus bounded context.
- `src/telemetry/queue.rs`: continue reporting SDK-only failures without HTTP context.
- `tests/startup.rs`: preserve process-level direct diagnostic expectations.
- `docs/operations.md`: document HTTPS transport and safe per-export fields.
- `changelog/2026-08-10T20-01-04-0400-otlp-https-failure-diagnostics.md`: record behavior and validation evidence.

---

### Task 1: Compile and prove HTTPS transport support

**Files:**
- Modify: `src/telemetry.rs:566-829`
- Modify: `Cargo.toml:45`
- Modify: `Cargo.lock`

**Interfaces:**
- Consumes: `build_blocking_http_client(timeout: Duration) -> Result<reqwest::blocking::Client, ()>`.
- Produces: the same function and client type, now capable of initiating HTTPS with rustls.

- [ ] **Step 1: Add a local failing HTTPS capability test**

In `src/telemetry.rs`, add `net::TcpListener` and `thread` to the test module's `std` imports
(the module already imports `io` and `time::{Duration, Instant}`), add
`build_blocking_http_client` to the `super` imports, and add:

```rust
#[test]
fn blocking_http_client_attempts_https_transport() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("test listener binds");
    listener
        .set_nonblocking(true)
        .expect("test listener becomes nonblocking");
    let address = listener
        .local_addr()
        .expect("test listener address is available");
    let accepted = thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            match listener.accept() {
                Ok((_stream, _peer)) => return true,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return false;
                    }
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) => panic!("test listener failed: {error}"),
            }
        }
    });
    let client = build_blocking_http_client(Duration::from_secs(1))
        .expect("blocking HTTP client builds");

    let result = client.get(format!("https://{address}")).send();

    assert!(result.is_err(), "the fixture is not a TLS server");
    assert!(
        accepted.join().expect("test listener does not panic"),
        "the production client must open a connection for an HTTPS URL"
    );
}
```

The raw TCP fixture intentionally closes during TLS negotiation. A TLS-capable client reaches the
listener; the current client rejects HTTPS before opening a connection.

- [ ] **Step 2: Run the test and verify RED**

Run:

```bash
cargo test telemetry::tests::blocking_http_client_attempts_https_transport --lib -- --exact
```

Expected: FAIL at `the production client must open a connection for an HTTPS URL` after the local
listener deadline because no TLS backend is compiled.

- [ ] **Step 3: Enable rustls on the existing reqwest dependency**

Change the direct dependency in `Cargo.toml` to:

```toml
reqwest = { version = "0.13", default-features = false, features = ["blocking", "rustls"] }
```

Then resolve only the lockfile changes required by that feature:

```bash
cargo update -p reqwest --precise 0.13.4
```

Do not add a second reqwest declaration or enable all default features.

- [ ] **Step 4: Verify GREEN and inspect the feature graph**

Run:

```bash
cargo test telemetry::tests::blocking_http_client_attempts_https_transport --lib -- --exact
cargo tree -e features -i reqwest
cargo tree | rg 'rustls|hyper-rustls|tokio-rustls'
```

Expected: the test passes; the feature tree contains `reqwest feature "rustls"`; the dependency tree
contains rustls transport crates and no native-tls/OpenSSL dependency.

- [ ] **Step 5: Commit the HTTPS deliverable**

```bash
git add Cargo.toml Cargo.lock src/telemetry.rs
git commit -m "fix: enable TLS for OTLP exporters"
```

---

### Task 2: Emit bounded context for every export failure

**Files:**
- Modify: `src/telemetry/diagnostics.rs:1-340`
- Modify: `src/telemetry/http_client.rs:1-255`
- Verify: `src/telemetry/queue.rs:160-185`
- Verify: `tests/startup.rs:180-210`

**Interfaces:**
- Consumes: `TelemetrySignal`, `TelemetryExportFailureReason`, and existing direct diagnostic sink.
- Produces: `ExportFailureContext`, `ExportFailureDetail`, and
  `DiagnosticsObserver::export_failure_with_context(signal, reason, context)`.
- Preserves: `DiagnosticsObserver::export_failure(signal, reason)` for SDK, shutdown, timeout, and
  encoding callers that have no HTTP metadata.

- [ ] **Step 1: Add failing diagnostics tests for per-failure output and bounded context**

In `src/telemetry/diagnostics.rs`, add the desired closed types to the test imports and replace the
failure-rate-limit assertion with these tests:

```rust
#[test]
fn every_export_failure_is_reported() {
    let metrics = Metrics::new();
    let sink = Arc::new(CaptureSink::default());
    let observer = DiagnosticsObserver::with_dependencies(
        metrics.clone(),
        Arc::new(TestClock::default()),
        sink.clone(),
    );

    for _ in 0..3 {
        observer.export_failure(
            TelemetrySignal::Trace,
            TelemetryExportFailureReason::Timeout,
        );
    }

    assert_eq!(
        sink.lines(),
        vec![
            "telemetry pipeline diagnostic kind=failure signal=trace reason=timeout\n";
            3
        ]
    );
    assert!(metrics.encode().expect("metrics encode").contains(
        "github_telemetry_export_failures_total{signal=\"trace\",reason=\"timeout\"} 3"
    ));
}

#[test]
fn export_failure_context_uses_only_bounded_fields() {
    let sink = Arc::new(CaptureSink::default());
    let observer = DiagnosticsObserver::with_dependencies(
        Metrics::new(),
        Arc::new(TestClock::default()),
        sink.clone(),
    );

    observer.export_failure_with_context(
        TelemetrySignal::Log,
        TelemetryExportFailureReason::HttpResponse,
        ExportFailureContext::with_status(401),
    );
    observer.export_failure_with_context(
        TelemetrySignal::Log,
        TelemetryExportFailureReason::Transport,
        ExportFailureContext::with_detail(ExportFailureDetail::RequestBuilder),
    );

    assert_eq!(
        sink.lines(),
        vec![
            "telemetry pipeline diagnostic kind=failure signal=log reason=http_response status=401\n",
            "telemetry pipeline diagnostic kind=failure signal=log reason=transport detail=request_builder\n",
        ]
    );
}
```

Keep the existing concurrent queue-drop test; it is the regression proving drop limiting remains.

- [ ] **Step 2: Run diagnostics tests and verify RED**

Run:

```bash
cargo test telemetry::diagnostics::tests --lib
```

Expected: compilation fails because `ExportFailureContext`, `ExportFailureDetail`, and
`export_failure_with_context` do not exist; the old implementation also emits only one failure line
with `suppressed=0`.

- [ ] **Step 3: Implement the closed context and remove only failure limiting**

In `src/telemetry/diagnostics.rs`, add:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ExportFailureDetail {
    Connect,
    RequestBuilder,
    Redirect,
    Request,
}

impl ExportFailureDetail {
    fn as_str(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::RequestBuilder => "request_builder",
            Self::Redirect => "redirect",
            Self::Request => "request",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct ExportFailureContext {
    status: Option<u16>,
    detail: Option<ExportFailureDetail>,
}

impl ExportFailureContext {
    pub(super) fn with_status(status: u16) -> Self {
        Self {
            status: Some(status),
            detail: None,
        }
    }

    pub(super) fn with_detail(detail: ExportFailureDetail) -> Self {
        Self {
            status: None,
            detail: Some(detail),
        }
    }
}
```

Remove `FAILURE_CATEGORY_COUNT` and `failure_limiters` from `DiagnosticsInner`. Preserve
`DROP_CATEGORY_COUNT`, `drop_limiters`, `Clock`, and `CategoryLimiter` unchanged for drops.
Implement failure recording as:

```rust
pub(super) fn export_failure(
    &self,
    signal: TelemetrySignal,
    reason: TelemetryExportFailureReason,
) {
    self.export_failure_with_context(signal, reason, ExportFailureContext::default());
}

pub(super) fn export_failure_with_context(
    &self,
    signal: TelemetrySignal,
    reason: TelemetryExportFailureReason,
    context: ExportFailureContext,
) {
    self.inner
        .metrics
        .record_telemetry_export_failure(signal, reason);
    let mut line = format!(
        "telemetry pipeline diagnostic kind=failure signal={} reason={}",
        signal.as_str(),
        reason.as_str()
    );
    if let Some(status) = context.status {
        line.push_str(&format!(" status={status}"));
    }
    if let Some(detail) = context.detail {
        line.push_str(&format!(" detail={}", detail.as_str()));
    }
    line.push('\n');
    drop(self.inner.sink.write(&line));
}
```

Keep `drop_records` calling a drop-specific helper that uses `claim_report` and retains the existing
`kind=drop ... suppressed=<count>` format. Delete `failure_index`; retain `signal_index` and
`drop_index`.

- [ ] **Step 4: Run diagnostics tests and verify GREEN**

Run:

```bash
cargo test telemetry::diagnostics::tests --lib
```

Expected: all diagnostics tests pass, every failure line is present, failure metrics equal three,
and concurrent drop suppression remains exact.

- [ ] **Step 5: Add failing typed reqwest classification tests**

In `src/telemetry/http_client.rs`, change the classification test expectations to a result carrying
`reason` and `context`. Add actual reqwest errors for HTTP status, timeout, refused connection,
unsupported-scheme request construction, and redirect policy:

```rust
assert_eq!(
    classify_reqwest_error(&status),
    ClassifiedHttpFailure::with_status(
        TelemetryExportFailureReason::HttpResponse,
        503,
    )
);
assert_eq!(
    classify_reqwest_error(&timeout),
    ClassifiedHttpFailure::new(TelemetryExportFailureReason::Timeout)
);
assert_eq!(
    classify_reqwest_error(&transport),
    ClassifiedHttpFailure::with_detail(
        TelemetryExportFailureReason::Transport,
        ExportFailureDetail::Connect,
    )
);
```

Create a builder error with:

```rust
let builder = reqwest::blocking::Client::new()
    .get("ftp://collector.invalid/v1/logs")
    .send()
    .expect_err("unsupported transport fails request construction");
assert_eq!(
    classify_reqwest_error(&builder),
    ClassifiedHttpFailure::with_detail(
        TelemetryExportFailureReason::Transport,
        ExportFailureDetail::RequestBuilder,
    )
);
```

Create a redirect error using the existing local listener, one `302` response, and a client built
with `reqwest::redirect::Policy::custom(|attempt| attempt.error("blocked redirect"))`; assert the
bounded detail is `Redirect`. Add an invalid HTTP response fixture and assert any remaining typed
request failure maps to `Request`. Assert only enum values and status numbers, never rendered error
text.

- [ ] **Step 6: Run HTTP-client tests and verify RED**

Run:

```bash
cargo test telemetry::http_client::tests --lib
```

Expected: compilation fails because `ClassifiedHttpFailure` and context-aware classification do not
exist.

- [ ] **Step 7: Implement typed classification and observer wiring**

In `src/telemetry/http_client.rs`, import the new context/detail types and add:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClassifiedHttpFailure {
    reason: TelemetryExportFailureReason,
    context: ExportFailureContext,
}

impl ClassifiedHttpFailure {
    fn new(reason: TelemetryExportFailureReason) -> Self {
        Self {
            reason,
            context: ExportFailureContext::default(),
        }
    }

    fn with_status(reason: TelemetryExportFailureReason, status: u16) -> Self {
        Self {
            reason,
            context: ExportFailureContext::with_status(status),
        }
    }

    fn with_detail(
        reason: TelemetryExportFailureReason,
        detail: ExportFailureDetail,
    ) -> Self {
        Self {
            reason,
            context: ExportFailureContext::with_detail(detail),
        }
    }
}
```

Change `ObservingHttpClient::record_failure` to accept `ClassifiedHttpFailure`, increment
`classified_failures`, and call `export_failure_with_context`. Replace classification with:

```rust
fn classify_reqwest_error(error: &reqwest::Error) -> ClassifiedHttpFailure {
    if error.is_timeout() {
        ClassifiedHttpFailure::new(TelemetryExportFailureReason::Timeout)
    } else if let Some(status) = error.status() {
        ClassifiedHttpFailure::with_status(
            TelemetryExportFailureReason::HttpResponse,
            status.as_u16(),
        )
    } else {
        let detail = if error.is_connect() {
            ExportFailureDetail::Connect
        } else if error.is_builder() {
            ExportFailureDetail::RequestBuilder
        } else if error.is_redirect() {
            ExportFailureDetail::Redirect
        } else {
            ExportFailureDetail::Request
        };
        ClassifiedHttpFailure::with_detail(
            TelemetryExportFailureReason::Transport,
            detail,
        )
    }
}
```

For non-reqwest `HttpError`, record `ClassifiedHttpFailure::new(Transport)`. For invalid successful
OTLP protobuf responses, record `ClassifiedHttpFailure::new(Encoding)`. Return every original error
unchanged.

Do not change `src/telemetry/queue.rs`: its SDK-only failures must continue calling the context-free
`export_failure`, and its classified-failure sequence prevents duplicate counting.

- [ ] **Step 8: Verify diagnostics, HTTP client, queue accounting, and startup contracts**

Run:

```bash
cargo test telemetry::diagnostics::tests --lib
cargo test telemetry::http_client::tests --lib
cargo test telemetry::queue::tests --lib
cargo test --test startup
```

Expected: all pass. Queue tests prove each HTTP failure is counted once rather than again as
`internal`; startup tests still recognize direct failure diagnostics without depending on a
`suppressed` failure field.

- [ ] **Step 9: Commit the diagnostics deliverable**

```bash
git add src/telemetry/diagnostics.rs src/telemetry/http_client.rs tests/startup.rs
git commit -m "feat: add bounded OTLP failure details"
```

Include `tests/startup.rs` only if its exact expected failure-line assertion required adjustment.

---

### Task 3: Document, audit, and validate the complete change

**Files:**
- Modify: `docs/operations.md:350-415`
- Create: `changelog/2026-08-10T20-01-04-0400-otlp-https-failure-diagnostics.md`
- Verify: `src/telemetry/otlp_test.rs`
- Verify: `tests/startup.rs`

**Interfaces:**
- Consumes: rustls-backed existing exporter and bounded per-failure diagnostics from Tasks 1-2.
- Produces: operator-facing HTTPS and diagnostic contracts plus complete validation evidence.

- [ ] **Step 1: Update the operations reference**

In `docs/operations.md`, revise Remote telemetry to state:

```markdown
HTTPS OTLP endpoints use the bundled rustls client and do not require OpenSSL in the runtime image.

Every failed export writes one direct stderr line and increments one bounded Prometheus series.
HTTP failures may include `status=<code>`. Transport failures may include only
`detail=connect|request_builder|redirect|request`. Raw errors, endpoint URLs, headers, credentials,
request payloads, and collector response bodies are never written. Queue-drop lines remain limited
to one per signal/reason per monotonic minute and report the suppressed count on the next permitted
line.
```

Remove the old statement that all direct diagnostic categories, including failures, are limited to
one line per minute. Keep the existing metric interpretation guidance.

- [ ] **Step 2: Add the required timestamped changelog record**

Create `changelog/2026-08-10T20-01-04-0400-otlp-https-failure-diagnostics.md` with:

```markdown
# OTLP HTTPS and Failure Diagnostics

## Changed

- Enabled rustls on the existing blocking reqwest OTLP client so HTTPS trace and log endpoints can
  establish TLS without OpenSSL in the runtime image.
- Added bounded HTTP status and transport-detail fields to direct exporter failure diagnostics.
- Changed exporter failures to emit one direct stderr line per failed attempt while retaining
  rate-limited queue-drop diagnostics.

## Security

Diagnostics continue to exclude endpoint URLs, headers, credentials, request payloads, collector
response bodies, and raw dependency errors. They bypass tracing and the OTLP log pipeline.

## Validation

- `cargo test`
- `cargo build`
- `cargo clippy --all-targets -- -D warnings`
- `cargo fmt --check`
- `cargo doc --no-deps`
```

- [ ] **Step 3: Format and run focused privacy/resilience tests**

Run:

```bash
cargo fmt
cargo test telemetry::otlp_test::collector_outage_is_counted_without_affecting_webhook_or_readiness --lib -- --exact
cargo test telemetry::otlp_test::collector_http_failure_is_classified_without_exposing_response_body --lib -- --exact
cargo test telemetry::otlp_test::blocked_exporters_preserve_exact_bounds_and_export_otlp_protobuf --lib -- --exact
cargo test --test startup
```

Expected: all pass; HTTP status remains correctly classified; response bodies remain absent; direct
diagnostics do not recursively enter OTLP logs.

- [ ] **Step 4: Run the complete required verification suite**

Run in this order and inspect every exit status:

```bash
cargo fmt --check
cargo test
cargo build
cargo clippy --all-targets -- -D warnings
cargo doc --no-deps
```

Expected: every command exits zero with no warnings. If the repository's `justfile` defines stricter
wrappers, also run `just fmt` and `just test` before opening the PR.

- [ ] **Step 5: Audit dependency and secret boundaries**

Run:

```bash
cargo tree -e features -i reqwest
cargo tree | rg '(^|[[:space:]├└│])+(native-tls|openssl|openssl-sys) v' && exit 1 || true
rg -n 'Display|source\(|to_string\(\)' src/telemetry/diagnostics.rs src/telemetry/http_client.rs
rg -n 'sentry_key|x-sentry-auth|ingest\.[[:alnum:].-]*sentry\.io' \
  Cargo.toml Cargo.lock src tests docs/operations.md charts changelog
git diff --check origin/main...HEAD
```

Expected: reqwest has `blocking` and `rustls`; native-tls/OpenSSL linkage is absent; no raw error
rendering was added to the diagnostic path; no Sentry endpoint or key material is present in product
files; diff check is clean.

- [ ] **Step 6: Commit documentation and final validation record**

```bash
git add docs/operations.md changelog/2026-08-10T20-01-04-0400-otlp-https-failure-diagnostics.md
git commit -m "docs: describe OTLP HTTPS diagnostics"
```

- [ ] **Step 7: Prepare and open the pull request**

Confirm the branch contains only the design and implementation commits:

```bash
git status --short
git log --oneline origin/main..HEAD
git diff --stat origin/main...HEAD
```

Push the current branch and open a PR with a title such as
`fix: enable TLS and improve OTLP failure diagnostics`. The PR body must summarize the missing TLS
root cause, bounded diagnostic fields, privacy constraints, and exact verification commands run.
