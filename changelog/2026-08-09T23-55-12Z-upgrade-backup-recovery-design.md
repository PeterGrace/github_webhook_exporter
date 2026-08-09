# Upgrade, backup, and recovery design

- Defined a fail-closed, non-root SQLite maintenance Pod for online backup and offline restore.
- Chose a digest-pinned SQLite image without changing the minimal production image or Helm chart.
- Planned real Kind recovery checks for restore preconditions, ownership, permissions, encrypted
  repository use, delivery deduplication, merge-queue persistence, metrics, and privacy.
- Recorded normal and Recreate-equivalent singleton upgrade procedures and rollback boundaries.
