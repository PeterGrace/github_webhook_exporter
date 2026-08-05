# Bounded merge-queue metrics

- Added fixed Rust label vocabularies for merge-group actions and reasons, pull-request queue outcomes and reasons, and queue transition failures.
- Added exact, case-sensitive destroyed-reason normalization that discards unsupported raw values as `other`.
- Added merge-group event, queue outcome, queue duration, and transition-failure Prometheus families to startup exposition.
- Added a narrow completion API that rejects negative durations and durations above the inclusive 365-day sanity ceiling, recording only `invalid_duration` for rejected observations.
- Added the bounded `queue_state` webhook processing-failure stage.
- Added table-driven, boundary, concurrency, startup exposition, and sensitive-value leakage tests.
