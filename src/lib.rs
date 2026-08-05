//! Runtime foundation for the GitHub webhook exporter.

/// Authenticated HTTP APIs.
pub mod api;
/// HTTP application state, router construction, and server lifecycle primitives.
pub mod app;
/// Typed, validated, and redacted runtime configuration.
pub mod config;
/// Validated delivery, repository, and merge-queue domain values.
pub mod domain;
/// Safe HTTP-facing application errors.
pub mod error;
/// Unauthenticated process liveness and SQLite readiness endpoints.
pub mod health;
/// Operating-system signal normalization for graceful process shutdown.
pub mod lifecycle;
/// Bounded Prometheus instruments and label normalization.
pub mod metrics;
/// Scheduled processed-delivery and completed merge-queue-attempt retention.
pub mod retention;
/// Cryptographic and administrative-authentication security primitives.
pub mod security;
/// SQLite startup, migrations, probes, and durable repository, delivery, and queue persistence.
pub mod storage;
/// Local structured tracing initialization.
pub mod telemetry;
