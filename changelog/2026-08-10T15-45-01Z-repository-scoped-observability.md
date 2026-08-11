# Repository-Scoped Observability

Implemented GitHub enhancement issue #64.

## Changes

- Added the authenticated canonical full repository name as `repository` on every repository-scoped webhook, merge-group, merge-queue, and workflow-job Prometheus family.
- Preserved the complete lowercase `owner/repository` form instead of reducing identity to the repository segment.
- Added the fixed `repository="unknown"` fallback for requests that fail before authentication and for startup seed series.
- Kept the repository-configuration gauge and OTLP pipeline diagnostic metrics process-wide.
- Added a request-local authenticated repository context shared by Prometheus request accounting and OpenTelemetry HTTP root spans.
- Added `github.repository.name` to authenticated webhook `http.request` roots while preserving existing child-span attributes.
- Updated webhook, merge-queue, workflow, tracing, and operations documentation.

## Security and cardinality

Repository identity enters metrics and root traces only after HMAC authentication succeeds. Unauthenticated payload values cannot create arbitrary repository series or spoof a root trace attribute. Delivery IDs, pull-request numbers, SHAs, payload fields, secrets, signatures, and headers remain excluded from Prometheus labels.

## Validation

- Added a two-repository integration test covering canonical full-name separation.
- Added pre-authentication `unknown` and process-wide metric exclusions to regression coverage.
- Extended OTLP tests to verify repository identity on authenticated HTTP roots.
- Updated privacy tests to permit only the authenticated repository metric label while retaining all other redaction boundaries.
