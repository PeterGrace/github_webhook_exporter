# Example Grafana Dashboard Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an importable Grafana 10+ dashboard that provides an operational overview and detailed coverage of all 15 Prometheus metric families emitted by the exporter.

**Architecture:** Commit one standalone dashboard JSON file and one companion README under `examples/grafana/`. A Rust integration test parses the artifact with the repository's existing `serde_json` dependency and enforces its title, rows, template variables, Prometheus datasource use, filter use, and complete metric-family coverage without introducing dashboard-generation tooling.

**Tech Stack:** Grafana dashboard JSON, PromQL, Rust integration tests, `serde_json`, Cargo.

## Global Constraints

- Target Grafana 10 or newer and the standard Prometheus datasource plugin.
- Define `${datasource}`, `${job}`, `${instance}`, and `${repository}` variables; `job`, `instance`, and `repository` support multi-value and `All`.
- Apply `job=~"$job"` and `instance=~"$instance"` to every metric selector.
- Apply `repository=~"$repository"` to repository-scoped webhook, merge-queue, and workflow selectors, but not to global repository-count or telemetry-health selectors.
- Use `$__rate_interval` for rates and `$__range` for selected-range totals.
- Cover every one of the 15 metric families registered in `src/metrics.rs`.
- Add no Grafana provisioning, alerts, recording rules, Helm integration, or generation dependency.
- Document each implementation iteration in a timestamped file under `changelog/`.

---

### Task 1: Dashboard contract and standalone JSON

**Files:**
- Create: `tests/grafana_dashboard.rs`
- Create: `examples/grafana/github-webhook-exporter.json`

**Interfaces:**
- Consumes: the exact metric names registered by `Metrics::new()` in `src/metrics.rs`.
- Produces: an importable Grafana dashboard with UID `github-webhook-exporter`, title `GitHub Webhook Exporter`, and datasource variable `datasource`.

- [ ] **Step 1: Write the failing artifact contract test**

Create `tests/grafana_dashboard.rs`. Read the dashboard through
`include_str!("../examples/grafana/github-webhook-exporter.json")` and parse it as
`serde_json::Value`. Add focused tests that assert:

```rust
const REQUIRED_METRICS: [&str; 15] = [
    "github_webhook_requests_total",
    "github_webhook_events_total",
    "github_webhook_processing_duration_seconds",
    "github_webhook_request_body_bytes",
    "github_webhook_duplicates_total",
    "github_webhook_processing_failures_total",
    "github_repository_configurations",
    "github_merge_group_events_total",
    "github_merge_queue_pr_outcomes_total",
    "github_merge_queue_attempt_duration_seconds",
    "github_merge_queue_transition_failures_total",
    "github_workflow_job_steps",
    "github_workflow_job_trace_rejections_total",
    "github_telemetry_export_failures_total",
    "github_telemetry_dropped_records_total",
];
```

The parsed root must have title `GitHub Webhook Exporter`, UID `github-webhook-exporter`, and
`schemaVersion >= 39`. Its templating list must contain variable names `datasource`, `job`, and
`instance`; the latter two must have `multi: true` and `includeAll: true`. The row panel titles must
be exactly `Operational overview`, `Webhook details`, `Merge queue details`, and
`Workflow and telemetry details`.

Recursively collect every target `expr` string. Assert each required metric name occurs in the
joined expressions. For every target, assert `datasource.uid == "${datasource}"`; for every
non-empty expression, assert it contains both `job=~\"$job\"` and
`instance=~\"$instance\"`. This test intentionally validates observable dashboard behavior rather
than exact panel IDs or formatting.

- [ ] **Step 2: Run the focused test and verify RED**

Run:

```bash
cargo test --test grafana_dashboard -- --nocapture
```

Expected: compilation fails because `examples/grafana/github-webhook-exporter.json` does not exist.

- [ ] **Step 3: Add the minimal importable dashboard JSON**

Create a Grafana JSON document with `schemaVersion: 39`, `uid: "github-webhook-exporter"`,
`title: "GitHub Webhook Exporter"`, refresh interval `30s`, default time range `now-6h` to `now`,
and editable graph panels. Define:

```json
{
  "templating": {
    "list": [
      {"name":"datasource","type":"datasource","query":"prometheus"},
      {"name":"job","type":"query","query":{"query":"label_values(github_webhook_requests_total, job)"},"multi":true,"includeAll":true,"allValue":".*"},
      {"name":"instance","type":"query","query":{"query":"label_values(github_webhook_requests_total{job=~\"$job\"}, instance)"},"multi":true,"includeAll":true,"allValue":".*"}
    ]
  }
}
```

Every query target uses datasource object
`{"type":"prometheus","uid":"${datasource}"}`. Use the shared selector fragment
`{job=~"$job", instance=~"$instance"}` and add bounded metric labels inside the same selector.

Create these rows and panels:

1. `Operational overview`
   - `Webhook request rate`: `sum by (result) (rate(github_webhook_requests_total{...}[$__rate_interval]))`
   - `Accepted requests`: guarded accepted-rate divided by total-rate, displayed as percent.
   - `p95 processing latency`: `histogram_quantile(0.95, sum by (le) (rate(github_webhook_processing_duration_seconds_bucket{...}[$__rate_interval])))`
   - `Duplicate rate`: sum of `rate(github_webhook_duplicates_total{...}[$__rate_interval])`.
   - `Configured repositories`: sum of `github_repository_configurations{...}`.
   - `Processing failure rate`: sum by `stage` of the processing-failure counter rate.
   - `Telemetry export failure rate`: sum by `signal, reason` of the export-failure counter rate.
   - `Dropped telemetry rate`: sum by `signal, reason` of the dropped-record counter rate.

2. `Webhook details`
   - Event/action rates from `github_webhook_events_total`.
   - p50/p95 request-body bytes from `github_webhook_request_body_bytes_bucket`.
   - p50/p95 processing duration by result from
     `github_webhook_processing_duration_seconds_bucket`.
   - Selected-range requests by result using `increase(...[$__range])`.
   - Selected-range duplicates using `increase(...[$__range])`.
   - Selected-range failures by stage using `increase(...[$__range])`.

3. `Merge queue details`
   - Merge-group event rate by action/reason.
   - Selected-range pull-request outcomes by outcome/reason.
   - p50/p95 attempt duration by outcome from
     `github_merge_queue_attempt_duration_seconds_bucket`.
   - Selected-range transition failures by reason.

4. `Workflow and telemetry details`
   - p50/p95 workflow-job step count from `github_workflow_job_steps_bucket`.
   - Trace rejection rate by reason.
   - Selected-range OTLP export failures by signal/reason.
   - Selected-range dropped telemetry records by signal/reason.

Use Grafana `timeseries`, `stat`, `bargauge`, and `table` visualizations only where each matches the
query shape. Set seconds, bytes, percent, requests-per-second, and short-number units explicitly.
Descriptions must explain counter versus gauge behavior and avoid exactly-once claims.

- [ ] **Step 4: Run the focused test and verify GREEN**

Run:

```bash
cargo test --test grafana_dashboard -- --nocapture
```

Expected: all dashboard contract tests pass.

- [ ] **Step 5: Validate JSON syntax independently**

Run:

```bash
python3 -m json.tool examples/grafana/github-webhook-exporter.json >/dev/null
```

Expected: exit status 0 and no output.

- [ ] **Step 6: Commit the tested dashboard artifact**

```bash
git add tests/grafana_dashboard.rs examples/grafana/github-webhook-exporter.json
git commit -m "feat: add example Grafana dashboard"
```

### Task 2: Usage documentation and final project validation

**Files:**
- Create: `examples/grafana/README.md`
- Create: `changelog/2026-08-10T15-25-59Z-example-grafana-dashboard.md`

**Interfaces:**
- Consumes: `examples/grafana/github-webhook-exporter.json` and the chart's existing metrics Service/ServiceMonitor options.
- Produces: operator-facing import and filtering instructions plus the required iteration record.

- [ ] **Step 1: Write the companion README**

Document the exact workflow:

```markdown
# Example Grafana dashboard

This directory contains an importable Grafana 10+ dashboard for the Prometheus metrics emitted by
the GitHub Webhook Exporter.

1. Configure Prometheus to scrape `GET /metrics`. Helm users may enable
   `metrics.service.enabled` and `metrics.serviceMonitor.enabled`.
2. In Grafana, choose **Dashboards > New > Import** and upload
   `github-webhook-exporter.json`.
3. Select a Prometheus datasource, then narrow the dashboard with the `job` and `instance` filters.
```

Also state that the dashboard covers all current metric families, is an editable starting point,
and intentionally excludes provisioning, alerting, recording rules, and Helm-managed Grafana
resources.

- [ ] **Step 2: Write the timestamped implementation changelog**

Record the standalone JSON path, variable/filter behavior, four dashboard rows, all-family contract
test, documentation path, and validation commands in
`changelog/2026-08-10T15-25-59Z-example-grafana-dashboard.md`.

- [ ] **Step 3: Run the full validation sequence**

Run from the repository root, in order:

```bash
just fmt
cargo clippy --all-targets -- -D warnings
just test
python3 -m json.tool examples/grafana/github-webhook-exporter.json >/dev/null
git diff --check
```

Expected: every command exits 0; Cargo reports no warnings or test failures; the JSON parser emits
no output; and Git reports no whitespace errors.

- [ ] **Step 4: Inspect the final scope and metric coverage**

Run:

```bash
git status --short
git diff --stat HEAD
git grep -o 'github_[a-z0-9_]*' -- examples/grafana/github-webhook-exporter.json \
  | sort -u
```

Expected: only the README and changelog remain uncommitted after Task 1; the metric list includes all
15 required base family names (histograms may additionally appear with `_bucket` suffixes); and no
provisioning, alert, Helm, or dependency files were added.

- [ ] **Step 5: Commit documentation and validation record**

```bash
git add examples/grafana/README.md changelog/2026-08-10T15-25-59Z-example-grafana-dashboard.md
git commit -m "docs: explain example Grafana dashboard"
```

### Task 3: Repository dashboard filtering amendment

**Files:**
- Modify: `tests/grafana_dashboard.rs`
- Modify: `examples/grafana/github-webhook-exporter.json`
- Modify: `examples/grafana/README.md`
- Modify: `docs/superpowers/specs/2026-08-10-grafana-dashboard-design.md`
- Modify: `docs/superpowers/plans/2026-08-10-grafana-dashboard.md`
- Create: `changelog/2026-08-11T13-42-41Z-grafana-repository-filter.md`

**Interfaces:**
- Consumes: the `repository` label added by repository-scoped observability on `origin/main`.
- Produces: a dependent Grafana `repository` variable and repository-aware PromQL selectors.

- [ ] **Step 1: Extend the contract test and verify RED**

Require template variables in dependency order `datasource`, `job`, `instance`, `repository`.
Require `repository` to be multi-value with `All`, and require its query to discover repository
values from `github_webhook_requests_total` after job and instance filtering. Classify
`github_repository_configurations`, `github_telemetry_export_failures_total`, and
`github_telemetry_dropped_records_total` as global metrics. Assert every other target expression
contains `repository=~\"$repository\"`, while expressions using only global metrics do not.

Run `cargo test --test grafana_dashboard -- --nocapture` and expect failures for the absent variable
and absent repository selectors.

- [ ] **Step 2: Update the dashboard and verify GREEN**

Add the repository variable after `instance`:

```json
{
  "name": "repository",
  "label": "Repository",
  "type": "query",
  "datasource": {"type": "prometheus", "uid": "${datasource}"},
  "query": {
    "query": "label_values(github_webhook_requests_total{job=~\"$job\", instance=~\"$instance\"}, repository)"
  },
  "refresh": 1,
  "multi": true,
  "includeAll": true,
  "allValue": ".*"
}
```

Add `repository=~"$repository"` to every selector for webhook, merge-group, merge-queue, and
workflow metrics. Leave configured-repository and telemetry diagnostic selectors unchanged because
those families have no repository label. Run the focused test and JSON parser; expect both to pass.

- [ ] **Step 3: Update operator documentation and changelog**

Document the Repository filter, multi-select behavior, `unknown` semantics, and the exporter-global
scope of configured repository and telemetry panels. Record the merge from current `main`, query
changes, contract coverage, and validation in the timestamped changelog.

- [ ] **Step 4: Run full validation and commit**

Run `just fmt`, `cargo clippy --all-targets -- -D warnings`, `just test`, the Python JSON parser, and
`git diff --check`. Commit the amendment with `fix: filter dashboard by repository`, then push the
updated PR branch.
