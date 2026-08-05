# Bounded Prometheus metrics

- Added exact, case-sensitive v1 normalization for GitHub event types and actions, with closed result and failure-stage label types.
- Added a cloneable metrics component with narrow methods for webhook requests, newly claimed events, duplicates, processing failures, and repository counts.
- Added the unauthenticated `GET /metrics` OpenMetrics endpoint to the shared Axum application router.
- Added table-driven allowlist tests, metric update and sensitive-value leakage tests, shared-state concurrency coverage, and router exposition coverage.
