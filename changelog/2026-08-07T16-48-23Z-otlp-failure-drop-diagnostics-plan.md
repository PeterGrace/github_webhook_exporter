# OTLP failure and drop diagnostics implementation plan

- Split implementation into bounded metrics, direct rate-limited diagnostics, queue observation,
  structured HTTP/export classification, and end-to-end resilience validation.
- Required test-first red-green cycles for each production behavior.
- Locked the final validation gate to formatting, build, warning-free Clippy, all tests, and API
  documentation.
