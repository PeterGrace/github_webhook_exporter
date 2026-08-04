# Health checks and graceful lifecycle

- Added unauthenticated liveness and SQLite-backed readiness endpoints with empty, redacted
  responses.
- Added Tokio-native SIGINT and SIGTERM normalization and a bounded Axum request-drain path.
- Preserved fatal database startup and migration behavior before listener binding.
- Added controlled-time drain timeout tests, real in-flight request draining, process-level SIGTERM
  coverage, restart persistence coverage, and lifecycle/health redaction checks.
- Documented health semantics, startup failures, and shutdown timeout operation.
