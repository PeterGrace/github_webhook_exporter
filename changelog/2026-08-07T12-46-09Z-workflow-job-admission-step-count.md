# 2026-08-07 Workflow job admission step count

## Summary
- added a count-only `WorkflowJobAdmission` representation in `src/api/workflow_job.rs`
- counted `steps` with a custom Serde sequence visitor that consumes `IgnoredAny`
- treated missing `steps` as zero, rejected non-array `steps`, and validated positive workflow identifiers
- kept `project_completed_job(...)` behavior intact while reusing the validated admission identifiers and exact step-count capacity

## Verification
- `cargo test api::workflow_job::tests --lib`
- `cargo clippy --lib -- -D warnings`
- `cargo fmt --check`
