# SQLite maintenance contracts

- Added a fail-closed operator command for SQLite online backup and offline restore.
- Pinned the external SQLite 3.50.4 maintenance image by linux/amd64 digest.
- Kept the maintenance Pod non-root, capability-free, resource-bounded, and isolated from Secrets.
- Rejected restore until the StatefulSet requests zero replicas and its exporter pod is absent.
- Added shell tests for input validation, backup mechanics, restore ordering, ownership mode, and
  hardened Pod settings.
