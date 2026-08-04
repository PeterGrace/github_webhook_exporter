//! Runtime foundation for the GitHub webhook exporter.

/// Authenticated HTTP APIs.
pub mod api;
/// HTTP application state, router construction, and server lifecycle primitives.
pub mod app;
/// Typed, validated, and redacted runtime configuration.
pub mod config;
/// Validated application-domain values.
pub mod domain;
/// Safe HTTP-facing application errors.
pub mod error;
/// Cryptographic and administrative-authentication security primitives.
pub mod security;
/// SQLite startup, migrations, probes, and repository persistence.
pub mod storage;
/// Local structured tracing initialization.
pub mod telemetry;
