# Upgrade, Backup, and Recovery Design

## Goal

Give operators executable, tested procedures for singleton upgrades, SQLite online backup, offline
restore, and post-recovery verification without enlarging the production image, overlapping exporter
writers, exposing secrets, or copying an active database file.

## Scope

This change completes issue #48. It documents normal StatefulSet `RollingUpdate`, a safer
Recreate-equivalent scale-down upgrade, backup, restore, rollback limits, troubleshooting, and
disaster-recovery checks. It extends the disposable Kind acceptance suite to prove those procedures.
It does not add scheduled backups, point-in-time recovery, online replication, provider-specific
snapshot controllers, or multi-region recovery.

## Architecture

A focused Bash maintenance command will create a short-lived Kubernetes Pod mounting the existing
PVC. The Pod uses SQLite 3.50.4 from a linux/amd64 digest-pinned maintenance image and runs as the
same non-root UID/GID 65532 as the exporter. The production image and Helm chart gain no shell,
SQLite CLI, sidecar, Job, CronJob, or additional credential access.

The command has two operations:

- `backup` runs SQLite's online `.backup` operation while the exporter may remain available, checks
  the backup with `PRAGMA integrity_check`, and sets mode `0600`.
- `restore` first proves the StatefulSet has desired replicas zero and no exporter pod remains. It
  validates the backup, restores into a temporary database on the same volume, validates that
  database, sets mode `0600`, and atomically replaces the target database while removing stale WAL
  and shared-memory files. A pre-restore database is retained until the operator verifies recovery.

The script accepts namespace, StatefulSet, PVC, and backup filename as non-secret inputs. It uses the
caller's `KUBECONFIG` and optional `KUBECTL_CONTEXT`; names and filenames are strictly validated.
Maintenance pods have no Secret references, no service-account token, a read-only root filesystem,
no privilege escalation, all capabilities dropped, runtime-default seccomp, and bounded resources.

## Upgrade Flow

Normal environments may use the chart's one-replica StatefulSet `RollingUpdate` and wait for
readiness. Providers whose attachment transitions may overlap must use the documented safer flow:
scale to zero, wait for the exporter pod and volume attachment to release, run `helm upgrade`, then
scale to exactly one and wait for readiness. The singleton and optional `minAvailable: 0` PDB permit
intentional downtime. Storage-template changes and application/database downgrades are not promised
rollback paths.

## Recovery Acceptance Flow

The existing Kind lifecycle harness will:

1. Create repository configuration, a durable delivery claim, and pending merge-queue state.
2. Run the online maintenance backup and record only normalized evidence.
3. Prove restore refuses to run while the StatefulSet is active.
4. Mutate post-backup state, scale the StatefulSet to zero, and wait for the pod to disappear.
5. Restore the backup and verify UID/GID 65532 and mode `0600` before startup.
6. Perform a Recreate-equivalent Helm upgrade while zero replicas are active, restore exactly one
   replica, and observe at most one exporter process using the PVC.
7. Verify startup migrations and readiness, decrypt the pre-backup repository secret by accepting a
   correctly signed webhook, preserve pre-backup deduplication and queue state, and expose expected
   metrics.
8. Scan commands, rendered objects, logs, statuses, maintenance evidence, and diagnostics for
   generated credentials, signatures, and forbidden webhook or OTLP payload material.

The harness continues to own cluster cleanup and failure diagnostics. Static shell tests define the
maintenance command's fail-closed contracts before implementation.

## Error Handling and Safety

Every script uses strict shell mode and `umask 077`. Restore checks happen before any maintenance Pod
is created. Kubernetes object names and backup basenames are bounded to safe character sets. Pods
are replaced rather than reused, and cleanup never hides the original failure. Error output names
the failed stage but never prints credentials, payloads, database contents, or SQLite error details
that could contain persisted values.

A successful online backup is only a consistent SQLite artifact; it is not durable disaster recovery
until copied off the application PVC or protected by a platform snapshot. Documentation requires
operators to encrypt and retain that external copy according to local policy.

## Validation

- Maintenance unit tests fail before implementation and pass afterward.
- The Kind recovery flow creates and restores a real SQLite online backup.
- Restore safely rejects a running StatefulSet.
- Restored ownership, permissions, migrations, readiness, repository-secret decryption, signed
  webhook handling, metrics, deduplication, and queue persistence are verified.
- `just fmt`, `cargo build --locked`, `cargo clippy --all-targets -- -D warnings`, `just test`,
  `cargo doc --no-deps --locked`, ShellCheck, static Helm checks, and the runtime Kind suite pass.

## Alternatives Rejected

- Documentation-only commands would duplicate safety logic and allow docs and tests to drift.
- Chart-managed Jobs or CronJobs would expand scope into scheduling, retention, Secret policy, and
  production lifecycle management.
- Adding SQLite tooling to the production image would weaken the minimal runtime contract.
- Copying the live database file would not provide a supported consistent SQLite backup.
