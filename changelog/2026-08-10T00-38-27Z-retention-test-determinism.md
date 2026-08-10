# Retention test determinism

- Replaced periodic-scheduler polling in the single-pass delivery-failure retention test with the
  existing deterministic one-pass helper.
- Asserted the durable merge-queue result directly after a delivery-store failure in the same pass.
- Removed shutdown timing and incidental tracing capture from a test whose contract concerns neither
  scheduler cancellation nor log formatting; dedicated tests retain failure-log coverage.
