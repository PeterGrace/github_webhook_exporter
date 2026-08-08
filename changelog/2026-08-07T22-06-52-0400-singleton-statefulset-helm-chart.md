# Singleton StatefulSet Helm chart

- Added the versioned `github-webhook-exporter` Helm chart at chart version `0.1.0` and application
  version `0.1.0`.
- Added a fixed singleton StatefulSet with a `ReadWriteOnce` PVC for durable SQLite storage, plus a
  ClusterIP Service and the application's HTTP liveness and readiness probes.
- Applied the production image's non-root UID/GID 65532 contract, `fsGroup` storage permissions,
  runtime-default seccomp, a read-only root filesystem, disabled privilege escalation, dropped
  capabilities, and disabled ServiceAccount token mounting.
- Projected typed non-secret application, retention, and telemetry configuration through a
  ConfigMap while referencing required credentials and optional OTLP headers from one existing
  Secret.
- Added strict render-time checks for singleton storage, telemetry batch capacity, and a termination
  grace period greater than the application and telemetry shutdown timeout sum, without a
  `preStop` delay.
- Added an optional `policy/v1` PodDisruptionBudget with fixed `minAvailable: 0` semantics.
- Added Helm lint and structural render tests for chart defaults, overrides, schema failures,
  credential hygiene, security, probes, storage, Secret references, disruption, and lifecycle
  configuration boundaries.
- Added disposable Kind acceptance for the StatefulSet, Service, ConfigMap, and PVC Kubernetes APIs
  without waiting for image readiness or testing runtime lifecycle behavior.

This iteration does not add Ingress, NetworkPolicy, image publication, or runtime lifecycle behavior
testing. Required GHCR image publication remains tracked by #50.
