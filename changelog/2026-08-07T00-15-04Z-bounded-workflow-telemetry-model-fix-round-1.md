# Bounded workflow telemetry model fix round 1

- Tightened workflow timing construction so reported intervals, fallback intervals, and child
  intervals are validated before use.
- Added a bounded pull-request collection wrapper that preserves order and truncates to the first
  20 validated values.
- Replaced the workflow job transport-style constructor with a validated parts struct and removed
  the raw string workflow task-run attribute helper.
- Kept the trace policy centralized and added/updated focused regression tests for the new
  invariants.
