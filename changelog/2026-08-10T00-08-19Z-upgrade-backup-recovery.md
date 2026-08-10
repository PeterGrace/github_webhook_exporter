# Upgrade, backup, and recovery operations

- Added explicit Helm maintenance mode so upgrades and restores can preserve a zero-replica window.
- Added a digest-pinned, non-root SQLite online-backup and offline-restore command with fail-closed
  replica, pod, integrity, ownership, and permission checks.
- Extended Kind lifecycle acceptance to create a real backup, reject an unsafe restore, mutate and
  restore state, and verify migrations, readiness, encrypted repository use, signed webhooks,
  delivery deduplication, merge-queue persistence, metrics, and singleton recovery.
- Documented normal and stopped upgrades, off-PVC backup retention, restore steps, troubleshooting,
  disruption downtime, and database rollback limits.
- Added maintenance tests to CI and preserved privacy scanning across recovery artifacts.
