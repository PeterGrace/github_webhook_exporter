# Workflow job admission double-parse fix

## Summary
- Removed the redundant internal `inspect_completed_job(...)` call from `project_completed_job(...)`.
- Kept the standalone admission API available for the later handler-level first pass.
- Preserved detailed projection validation/output by validating `id`, `run_id`, and `run_attempt` directly from the detailed projection payload.
- Added a focused regression test proving detailed projection no longer triggers admission-step deserialization.

## Files
- `src/api/workflow_job.rs`
- `.superpowers/sdd/2026-08-07-workflow-job-step-limit/task-2-report.md`

## Verification
- `cargo test api::workflow_job::tests::detailed_projection_does_not_run_admission_deserialization --lib`
- `cargo fmt && cargo test api::workflow_job::tests --lib && cargo clippy --lib -- -D warnings`
- `cargo fmt --check && cargo build`
