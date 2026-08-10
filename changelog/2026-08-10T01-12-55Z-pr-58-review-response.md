# PR 58 review response

- Pinned online SQLite maintenance Pods to the running exporter's node for `ReadWriteOnce` PVC
  compatibility and documented the remaining CSI limitation.
- Routed SQLite temporary files to the writable `/data` mount while preserving a read-only root
  filesystem.
- Documented that restore preconditions are point-in-time checks requiring maintenance mode to stay
  enabled through completion.
- Kept the strict single-line maintenance success sentinel to reject unexpected stdout.
- Required merge-queue workload and completed outcome fields to co-occur on one captured log line
  without depending on tracing field order.
