//! Runtime foundation for the GitHub webhook exporter.

/// HTTP application state, router construction, and server lifecycle primitives.
pub mod app;
/// Typed, validated, and redacted runtime configuration.
pub mod config;
/// Safe HTTP-facing application errors.
pub mod error;
/// Bounded Prometheus instruments and label normalization.
pub mod metrics;
/// Local structured tracing initialization.
pub mod telemetry;
