# PR 58 review response

- Pinned online SQLite maintenance Pods to the running exporter's node for `ReadWriteOnce` PVC
  compatibility and documented the remaining CSI limitation.
- Routed SQLite temporary files to the writable `/data` mount while preserving a read-only root
  filesystem.
- Documented that restore preconditions are point-in-time checks requiring maintenance mode to stay
  enabled through completion.
- Kept the strict single-line maintenance success sentinel to reject unexpected stdout.
- Removed incidental tracing assertions from the same-pass retention behavior test and retained the
  stronger durable queue-state assertion; dedicated tests continue to cover failure logging.
