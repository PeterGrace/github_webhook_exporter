# 2026-08-07T11:14:44-0400 PR 39 OTLP series/label coverage fix

- Corrected `src/telemetry/otlp_test.rs` to inspect the Prometheus metric series/label segment
  before the trailing sample value when checking for leaked `workflow_run_id` and `workflow_job_id`.
- Kept the sample-value assertion as independent protection against numeric leakage.
- No production behavior changed.
