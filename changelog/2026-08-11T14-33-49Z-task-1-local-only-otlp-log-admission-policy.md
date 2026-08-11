# Task 1: Local-only OTLP log admission policy

- Added `LOCAL_ONLY_LOG_TARGET` for the local-only log target.
- Split OTLP metadata admission so the log bridge uses `is_remote_log_target`.
- Kept trace admission on the existing application namespace policy.
- Added focused regression coverage for the local-only target rejection.
