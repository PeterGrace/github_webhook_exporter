# How to back up and restore SQLite

The production image ships no shell and no SQLite client, so backup and restore both go through
`scripts/helm-sqlite-maintenance.sh`, which runs a separate, digest-pinned maintenance Pod against
the same PVC. Do not copy the live `github-webhook-exporter.db` file directly — SQLite may have
committed state sitting in its WAL, so a live file copy is unsupported and can be inconsistent.

## Back up online, while the exporter is running

```bash
backup_name="backup-$(date -u +%Y%m%dT%H%M%SZ).db"
scripts/helm-sqlite-maintenance.sh backup \
  github-webhook-exporter \
  github-webhook-exporter \
  data-github-webhook-exporter-0 \
  "${backup_name}"
```

This uses SQLite's online `.backup` operation, validates the result, and sets file mode `0600`.
Set `KUBECONFIG` and `KUBECTL_CONTEXT` first if you're not operating against your default context.

An online backup mounts the `ReadWriteOnce` PVC in a second Pod alongside the exporter's, so the
command pins the maintenance Pod to the exporter's current node — same-node multi-mount, not
cross-node. If your storage provider forbids that too, don't use this procedure; keep
`maintenanceMode=true` and take a coordinated offline platform snapshot instead.

The backup file lands on the application PVC first. Copy it to an encrypted, access-controlled
backup system, or take a provider snapshot, immediately after the command completes — a backup
that only ever exists on the application PVC does not protect you against losing that PVC.

## Restore

Stop the exporter and wait for its pod to fully disappear before restoring — the maintenance
script refuses to create a maintenance Pod otherwise:

```bash
helm upgrade github-webhook-exporter charts/github-webhook-exporter \
  --namespace github-webhook-exporter \
  --reuse-values \
  --set maintenanceMode=true \
  --wait
kubectl --namespace github-webhook-exporter wait --for=delete \
  pod/github-webhook-exporter-0 --timeout=180s

export BACKUP_NAME=backup-20260101T000000Z.db   # the backup you validated
scripts/helm-sqlite-maintenance.sh restore \
  github-webhook-exporter \
  github-webhook-exporter \
  data-github-webhook-exporter-0 \
  "${BACKUP_NAME}"
```

Restore validates the backup and the restored database, writes the replacement as UID/GID
`65532:65532` with mode `0600`, removes stale WAL/shared-memory files, and keeps the file it
replaced as `github-webhook-exporter.db.pre-restore`. Don't delete that file, or the source
backup, until you've accepted the recovery below.

Bring the exporter back up:

```bash
helm upgrade github-webhook-exporter charts/github-webhook-exporter \
  --namespace github-webhook-exporter \
  --reuse-values \
  --set maintenanceMode=false \
  --wait
kubectl --namespace github-webhook-exporter wait --for=condition=Ready \
  pod/github-webhook-exporter-0 --timeout=180s
kubectl --namespace github-webhook-exporter port-forward \
  service/github-webhook-exporter 8080:8080
```

## Accept the recovery

Using runtime-provided credentials — never ones typed into a command history or manifest — check
all of these before you delete the pre-restore file or the backup:

1. `GET /health/ready` returns `200`.
2. `GET /api/v1/repositories` lists the repository configuration you expect.
3. A correctly signed webhook using a pre-backup repository secret returns `204`.
4. `/metrics` exposes the repository gauge and the expected bounded webhook families.
5. Replaying a delivery ID from before the backup increments the duplicate counter, rather than
   being processed again.
6. A merge-queue attempt that was pending before the backup can still complete after recovery.

## If something goes wrong

- **Maintenance Pod reports a metadata failure**: keep `maintenanceMode=true` and fix the storage
  provider's UID/GID `65532` and permission behavior before retrying.
- **Readiness fails after restore**: inspect pod logs and PVC events; don't print the database or
  Secret contents while doing so.
- **Repository decryption fails after restore**: verify the original master-key Secret was
  restored too — a database backup alone cannot recover a lost encryption key.
- **You're tempted to `helm rollback`**: don't, for the database. Rollback restores rendered
  Kubernetes configuration, but it does not reverse SQLite migrations, storage-template changes,
  or an incompatible application downgrade. Recover those from a validated pre-upgrade backup with
  a compatible image instead.
