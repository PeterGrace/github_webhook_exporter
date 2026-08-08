# Helm Kind API acceptance

Added an isolated Kind acceptance check for the Helm chart.

- Creates a collision-resistant disposable cluster with an isolated kubeconfig.
- Generates required Secret values at runtime without printing them.
- Verifies the StatefulSet, Service, ConfigMap, PVC, singleton replica count, and pod UID through
  Kubernetes API reads without waiting for the application image.
- Uninstalls the Helm release, confirms its absence, and deletes only the cluster created by the
  check.
- Exposes the check as `just helm-kind-acceptance`.
