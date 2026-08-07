# Completed workflow-job OTLP traces

## Implementation

- Added an authenticated, bounded projection for newly claimed `workflow_job.completed` payloads.
- Added a direct SDK historical emitter with one independent `github.workflow.job` root and direct
  `github.workflow.step` children using explicit reported or fallback timestamps.
- Normalized conclusions, statuses, identifiers, pull-request arrays, and sanitized display names
  to the approved fixed attribute policy.
- Preserved at-most-once delivery admission and the existing non-blocking bounded trace queue.

## Privacy and failure isolation

- Kept approved workflow names and identifiers in spans only.
- Excluded commands, output and logs, actors, URLs, payload fragments, secrets, signatures, headers,
  and raw unknown conclusions from traces, OTLP logs, structured stderr, and Prometheus exposition.
- Added integrated in-process OTLP coverage for cross-signal privacy and centralized attribute/event
  allowlists.
- Added collector-unavailability coverage for the unchanged `204 No Content` response, readiness,
  generic metrics, and empty merge-queue state, using runtime failure counters without requiring a
  successful force flush against the unavailable endpoint.
- Documented completed-only admission, trace identity, timing, status, identifiers, name bounds,
  privacy, and collector-failure behavior in `docs/operations.md`.

## Final validation

Task 7 reran the full gate sequence from a clean state and all checks passed:

1. `just fmt` -> `cargo fmt --all -- --check` succeeded.
2. `cargo build` -> finished successfully with no warnings.
3. `cargo clippy --all-targets -- -D warnings` -> finished successfully with no warnings.
4. `just test` -> all tests passed.
5. `cargo doc --no-deps` -> documentation built successfully.

### Evidence from `just test`

- Hierarchy: `telemetry::workflow::tests::emitter_exports_independent_historical_job_and_step_spans`
  and `telemetry::otlp_test::workflow_job_completed_exports_one_independent_historical_trace`.
- Exact timestamps and fallback timing:
  `telemetry::workflow::tests::historical_timing_constructors_enforce_order_and_parent_bounds`,
  `telemetry::otlp_test::workflow_timing_uses_reported_and_bounded_fallback_intervals`,
  `api::workflow_job::tests::malformed_or_missing_job_timestamps_select_fallback`, and
  `api::workflow_job::tests::pre_epoch_receipt_times_project_with_checked_fallback`.
- Statuses and conclusion matrix:
  `telemetry::workflow::tests::workflow_conclusions_map_to_status_and_strings`,
  `telemetry::otlp_test::workflow_conclusions_export_bounded_results_and_statuses`, and
  `api::workflow_job::tests::reversed_job_timestamps_fall_back_at_valid_completion`.
- Unsupported or malformed input:
  `api::workflow_job::tests::malformed_or_non_array_steps_reject_projection`,
  `api::workflow_job::tests::unsupported_large_fields_have_no_representation_in_the_output_model`, and
  `telemetry::otlp_test::unsupported_workflow_actions_and_projections_emit_no_historical_trace`.
- Duplicates: `telemetry::otlp_test::duplicate_workflow_delivery_emits_one_historical_trace`.
- Collector unavailability:
  `telemetry::otlp_test::unavailable_collector_does_not_change_completed_workflow_response`.
- Privacy:
  `telemetry::otlp_test::integrated_core_trace_privacy`,
  `telemetry::otlp_test::workflow_identifiers_and_names_are_span_only_and_payload_data_is_absent`, and
  `telemetry::trace::tests::operation_spans_keep_sensitive_identifiers_out_of_fmt_output_and_export_otlp_attributes`.

### Gate summary

- `just fmt`: passed.
- `cargo build`: passed.
- `cargo clippy --all-targets -- -D warnings`: passed.
- `just test`: passed with 132 library tests, 0 `src/main.rs` tests, 8 delivery storage tests,
  10 merge queue storage tests, 18 repository API tests, 5 startup tests, 8 storage tests, and
  22 webhook API tests.
- `cargo doc --no-deps`: passed.
