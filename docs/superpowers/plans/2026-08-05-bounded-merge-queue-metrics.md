# Bounded Merge-Queue Metrics Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add fixed-vocabulary merge-group and merge-queue Prometheus metrics with exact reason normalization and safe duration validation.

**Architecture:** Extend the existing private, shared `MetricsInner` registry in `src/metrics.rs`. Public closed enums and narrow `Metrics` methods are the only update boundary; raw destroyed reasons pass through one exact classifier, while signed queue durations are validated before either completion metric is updated.

**Tech Stack:** Rust 2021, `prometheus-client` 0.25, `time` 0.3, built-in Rust tests, Axum integration tests.

## Global Constraints

- Every metric label must come from a closed Rust vocabulary.
- Raw reasons, repository names, pull-request numbers, delivery UUIDs, SHAs, URLs, and payload fragments must never become labels.
- Destroyed reasons match exactly and case-sensitively; unsupported values map to `other`.
- Queue durations below zero or above exactly 365 days must increment only `invalid_duration`.
- Existing Phase 2 metric names and labels remain unchanged except for the additive `queue_state` failure stage.
- Production code must follow failing-test-first TDD.

---

### Task 1: Closed Classifier and Failure Vocabularies

**Files:**
- Modify: `src/metrics.rs`

**Interfaces:**
- Produces: `MergeGroupAction`, `MergeGroupReason`, `MergeQueueOutcome`, `MergeQueueReason`, `QueueTransitionFailureReason`, and `normalize_merge_group_destroyed_reason(&str) -> MergeGroupReason`.
- Produces: `FailureStage::QueueState` encoded as `queue_state`.

- [ ] **Step 1: Write failing table-driven tests**

Add tests that require every fixed encoded value, exact destroyed-reason normalization, malicious-input collapse to `other`, and `FailureStage::QueueState` exposition.

- [ ] **Step 2: Verify RED**

Run `cargo test metrics::tests::normalize_merge_group -- --nocapture` and the vocabulary test filters. Expect compilation failures because the Phase 3 types and classifier do not exist.

- [ ] **Step 3: Implement the minimal vocabularies**

Define documented public enums with private `as_str` methods and `EncodeLabelValue` implementations. Add exact matching for `merged`, `dequeued`, and `invalidated`, with all other strings mapping to `other`. Extend `FailureStage` and its seeded labels with `QueueState`.

- [ ] **Step 4: Verify GREEN**

Run `cargo test metrics::tests -- --nocapture`. Expect all metrics unit tests to pass.

### Task 2: Phase 3 Metric Families and Safe Update APIs

**Files:**
- Modify: `src/metrics.rs`

**Interfaces:**
- Consumes: Task 1 fixed vocabularies.
- Produces: `Metrics::record_merge_group_event`, `Metrics::record_merge_queue_completion`, and `Metrics::record_merge_queue_transition_failure`.
- Produces: `github_merge_group_events_total`, `github_merge_queue_pr_outcomes_total`, `github_merge_queue_attempt_duration_seconds`, and `github_merge_queue_transition_failures_total`.

- [ ] **Step 1: Write failing behavior tests**

Add tests proving startup exposition, checks-requested reason coercion to `none`, valid completion counter/histogram updates, exact 365-day acceptance, negative and over-ceiling rejection, and concurrent clone updates.

- [ ] **Step 2: Verify RED**

Run `cargo test metrics::tests -- --nocapture`. Expect compilation failures because the methods and metric families do not exist.

- [ ] **Step 3: Implement minimal families and APIs**

Add typed label sets and private families to `MetricsInner`, seed bounded startup labels, register all four names, and validate `time::Duration` against `time::Duration::ZERO..=time::Duration::days(365)` before updating completion metrics. Invalid values update only `QueueTransitionFailureReason::InvalidDuration`.

- [ ] **Step 4: Verify GREEN**

Run `cargo test metrics::tests -- --nocapture`. Expect all metrics unit tests to pass without warnings.

### Task 3: HTTP Exposition and Leakage Regression Coverage

**Files:**
- Modify: `src/app.rs`
- Modify: `src/metrics.rs`
- Create: `changelog/2026-08-05T09-22-31-0400-bounded-merge-queue-metrics.md`

**Interfaces:**
- Consumes: Task 2 registered metrics.
- Produces: startup HTTP exposition and regression evidence for all issue acceptance criteria.

- [ ] **Step 1: Write failing endpoint and leakage assertions**

Extend the startup metrics endpoint test to require all four Phase 3 families and valid OpenMetrics content type. Extend the untrusted-values unit test to pass malicious destroyed reasons and verify no raw identifier or payload fragment appears.

- [ ] **Step 2: Verify RED**

Run the focused app and metrics tests. Expect missing-family or missing-coverage failures before completing seeding and classifier use.

- [ ] **Step 3: Complete startup seeding and documentation**

Make only the minimal implementation adjustments needed by the integration test. Add a timestamped changelog entry describing bounded labels, duration rejection, and validation.

- [ ] **Step 4: Run mandatory validation**

Run, in order: `just fmt`, `cargo build`, `cargo clippy --all-targets -- -D warnings`, `just test`, and `cargo doc --no-deps`. If any command fails, fix the issue and rerun the full sequence.

- [ ] **Step 5: Commit**

Stage the metrics, tests, plan, and changelog, then commit with `feat: add bounded merge-queue metrics` and `Closes #23` in the commit body.
