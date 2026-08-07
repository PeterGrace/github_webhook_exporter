# 2026-08-07 01:01:05 UTC - Workflow job claim-boundary trace dispatch

## Summary
- Wired `workflow_job.completed` projection and emission into the webhook handler only after authentication, normalized event/action selection, and a new durable delivery claim.
- Added an end-to-end OTLP hierarchy test covering independent historical workflow job export, direct child steps, sanitized names, exact timestamps, approved identifiers, and the 20 pull-request cap.
- Extended the OTLP test allowlist and helpers to validate workflow trace attributes and integer-array pull-request attributes.
