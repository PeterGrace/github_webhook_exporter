# Design vendor-neutral workflow exception events

- Defined OpenTelemetry `exception` span events as the canonical representation of failed and timed-out workflow tasks.
- Retained direct Sentry reporting as an optional promotion path for Sentry-native Issues.
- Specified shared bounded exception data, duplicate job suppression, privacy constraints, and verification coverage.
