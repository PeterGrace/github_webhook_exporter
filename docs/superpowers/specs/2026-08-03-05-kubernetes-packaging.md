# Specification 5: Kubernetes Packaging

## Goal

Package the completed service for safe singleton operation on Kubernetes with persistent SQLite
storage, correct probes, and shutdown behavior.

## Dependencies

Specifications 1, 2, and 4 must be complete. Specification 3 must also be complete when merge-queue
tracking is included in the deployed product.

## Workload

Provide versioned Kubernetes manifests or a Helm chart containing:

- A one-replica StatefulSet.
- A `ReadWriteOnce` persistent volume claim mounted at a non-root data directory.
- `GHE_DATABASE_PATH` pointing into that volume.
- A ClusterIP Service exposing the application port.
- HTTP liveness and readiness probes using `/health/live` and `/health/ready`.
- A termination grace period longer than the application's configured shutdown timeout.
- CPU and memory requests and limits with documented starting values.
- A pod security context that runs as non-root and grants database access through `fsGroup` or an
  equivalent storage-compatible mechanism.

Use StatefulSet `RollingUpdate` behavior with one replica and ensure the replacement pod cannot
write the same SQLite database concurrently. The default recommendation is `Recreate`-equivalent
operator behavior: scale the existing pod down before starting a replacement when the storage
provider can attach the volume to multiple nodes during transitions. Document this deployment step
rather than claiming Kubernetes alone prevents overlap.

## Configuration and secrets

- A Kubernetes Secret supplies `GHE_MASTER_KEY`, `GHE_ADMIN_TOKEN`, and optional OTLP headers.
- A ConfigMap or chart values supply non-secret application and telemetry configuration.
- Secret values never appear in rendered examples, annotations, command arguments, or logs.
- Configuration API network access is restricted separately from webhook and metrics access.

Ingress TLS termination is an explicit platform responsibility. Example NetworkPolicies allow:

- GitHub/ingress traffic to the webhook endpoint.
- Prometheus traffic to `/metrics`.
- Administrative traffic to `/api/v1/repositories` only from the designated management namespace
  or ingress.
- Egress to DNS and the configured OTLP collector.

Because Kubernetes NetworkPolicy cannot normally filter HTTP paths, separate Services/ingress rules
or an authorization proxy must enforce path-level administrative restrictions.

## Probes and lifecycle

- Liveness checks process serviceability and does not query external systems.
- Readiness checks SQLite migration state and a lightweight database query.
- OTLP collector state never affects either probe.
- SIGTERM initiates the application's bounded drain and telemetry flush; the default package does
  not add a `preStop` sleep.
- The pod disruption budget uses `minAvailable: 0` when supplied, acknowledging that a singleton
  cannot remain available during voluntary disruption.

## Persistence operations

Document platform-specific backup and restore procedures. At minimum:

- Backups use SQLite's online backup mechanism or a coordinated application stop; copying an active
  database file alone is unsupported.
- Restore occurs while the StatefulSet is scaled to zero.
- Restored files retain ownership and restrictive permissions.
- Recovery verification includes migrations, readiness, repository decryption, and a test webhook.

## Tests

- Rendered manifests pass schema validation and policy linting in CI.
- A disposable local cluster test installs the package and waits for readiness.
- Tests exercise repository configuration, a signed webhook, metrics scraping, pod restart, and
  queue-state persistence when specification 3 is enabled.
- An invalid database mount makes readiness fail without causing a false healthy rollout.
- A collector outage leaves readiness healthy.
- A rollout test observes that no two application processes write the volume concurrently.
- Static scans ensure rendered resources contain no example credentials or privileged containers.

## Acceptance criteria

- The package deploys one non-root StatefulSet replica with persistent SQLite storage.
- Probes reflect application and database health exactly as defined by prior specifications.
- Secrets are supplied through Kubernetes Secret references and are absent from manifests.
- Administrative API exposure is explicitly restricted.
- Upgrades prevent concurrent SQLite writers.
- Graceful shutdown completes within the pod termination grace period.
- Backup and restore procedures are documented and tested.
