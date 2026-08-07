# Authenticated Workflow Job Projection

## Summary
- Added `src/api/workflow_job.rs` with a bounded authenticated workflow-job projection that builds `WorkflowJobTrace` values without side effects.
- Registered the module in `src/api/mod.rs`.
- Covered required timing fallback, bounded PR retention, step ordering, malformed `steps` rejection, and pre-epoch receipt handling with focused unit tests.

## Notes
- Timestamp parsing accepts only RFC 3339 strings and safely converts `OffsetDateTime` to `SystemTime` with checked pre-epoch arithmetic.
- Job timing prefers reported ordered bounds, otherwise falls back to valid completion time or request receipt time.
- Step timing prefers reported in-parent bounds and otherwise falls back to the selected job end.
