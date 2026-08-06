# Task 3: Webhook and Merge-Queue Tracing

## Changed

- Added bounded `github.webhook.authenticate` and `github.webhook.process` request children.
- Added newly claimed `merge_queue.update` spans for supported merge-group and pull-request queue transitions.
- Added normalized webhook, merge-group, queue entity, outcome, reason, and failure-event attributes through the central trace policy.
- Added span-only repository, delivery, pull-request, and validated SHA identifiers.
- Accepted and lowercase-normalized full 40-character SHA-1 and 64-character SHA-256 identifiers; malformed values are omitted.
- Preserved duplicate suppression, queue persistence and metrics, redacted errors, and authenticated queue-failure `204` responses.

## Tests

- Added OTLP hierarchy, outcome, failure, duplicate, SHA validation, and cross-signal privacy coverage.
- Extended webhook integration payloads and redaction assertions with pull-request SHA values.
- Verified focused webhook OTLP and webhook API suites before the full project gates.
