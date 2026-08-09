# Kind lifecycle failure and rollout contracts

- Added a broken database-path pod that proves Kubernetes readiness never reports a false success
  and that normalized logs omit the configured path.
- Added controlled HTTP and one-second retention activity during SIGTERM, with bounded shutdown-stage
  and replacement timing assertions.
- Added Helm rollout sampling that proves at most one running exporter container references the
  SQLite PVC.
- Made artifact privacy scanning binary-safe and extended it to signatures and ignored payload
  material without printing matched values.
