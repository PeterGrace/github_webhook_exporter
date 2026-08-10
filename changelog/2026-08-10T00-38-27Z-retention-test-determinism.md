# Retention test determinism

- Replaced periodic-scheduler polling in the single-pass delivery-failure retention test with the
  existing deterministic one-pass helper.
- Preserved assertions that delivery failure is logged, merge-queue pruning completes in the same
  pass, and expired queue attempts are removed.
- Removed shutdown timing from a test whose contract does not concern scheduler cancellation.
- Asserted workload and outcome fields independently so tracing formatter field placement cannot
  obscure the tested semantic event.
