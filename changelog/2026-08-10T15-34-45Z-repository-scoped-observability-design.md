# Repository-Scoped Observability Design

- Defined authenticated full-name repository labeling for repository-scoped Prometheus metrics.
- Specified the fixed `repository="unknown"` fallback for pre-authentication outcomes.
- Kept process-wide repository-count and OTLP diagnostic metrics unchanged.
- Defined propagation of canonical repository identity to the root OpenTelemetry HTTP span.
- Documented security, cardinality, testing, and out-of-scope constraints.
