# Complete Phase 2 lifecycle and security regressions

- Initialize `github_repository_configurations` from durable SQLite state before binding and update
  it only after successful repository creates and deletes.
- Run processed-delivery retention on the configured Tokio interval, deleting expired claims in
  batches of at most 1,000 while preserving fresh claims.
- Coordinate Axum and retention through one cancellation signal and one bounded graceful-shutdown
  deadline.
- Add opaque UUID correlation IDs to safe internal/database error responses and matching structured
  logs without adding unbounded metric labels.
- Add controlled-time retention, startup gauge, shared-drain, restart, deduplication, correlation,
  and redaction regressions.
- Document retention operations, duplicate behavior, and the claim-before-counter crash boundary
  that prevents an exactly-once metrics guarantee.
