# Core service and merge-queue tracing

## Implementation

- Added the centralized `telemetry::trace` policy for stable operation names, bounded operational
  attributes, typed span-only identifiers, normalized status, and bounded failure events.
- Added request-rooted repository and webhook hierarchies covering repository writes,
  authentication, durable delivery claims, SQLite operations, merge-group processing, and
  pull-request merge-queue transitions.
- Added independent `retention.run` roots with `delivery.prune` and `merge_queue.prune` SQLite
  children. Retention tracing remains detached from ambient request spans.
- Preserved duplicate semantics: an authenticated duplicate records one process span with outcome
  `duplicate` and does not emit another specialized merge-queue update or outcome.
- Preserved failure and dequeue semantics: queue-state persistence failures retain the authenticated
  response and use bounded error status/event data; merge-group dequeue uses normalized reason
  `dequeued`; pull-request dequeue uses outcome `unknown` and reason `unclassified_dequeue`.
- Kept repository names and IDs, delivery IDs, pull-request numbers, and validated commit SHAs on
  spans only. Integrated coverage verifies their absence from OTLP logs, stderr, and Prometheus.
- Added an in-process privacy matrix with explicit resource, span, and event attribute allowlists.
  Unique sentinels cover secrets, signatures, authorization headers, actors, commands, raw reasons,
  raw URLs, raw unmatched paths, and payload fields across traces, OTLP logs, stderr, and
  Prometheus exposition. Dedicated failure regressions retain unique private storage sentinels.
- Made local webhook log records parentless so ordinary structured fields cannot become trace span
  events, while retaining the dedicated bounded `operation.failure` event.
- Stabilized OTLP and retention tests by initializing the SQLite trace callsite under the test
  dispatch, clearing only setup captures, dropping responses before capture, checkpointing burst
  scenarios, using condition-based waits, and widening diagnostic deadlines for slow CI.

## Validation

The final validation sequence completed in the required order with exit status 0 and no warnings:

1. `just fmt` — passed.
2. `cargo build` — passed.
3. `cargo clippy --all-targets -- -D warnings` — passed.
4. `just test` — passed: 172 tests, 0 failed.
5. `cargo doc --no-deps` — passed and generated the crate documentation.

`cargo test telemetry::otlp_test::integrated_core_trace_privacy --lib -- --nocapture` also passed
10 consecutive runs after the test-harness stabilization. The issue #10 handoff still names the
“Identifier boundary with #34”, workflow run ID, workflow job ID, and shared span-only helper.
