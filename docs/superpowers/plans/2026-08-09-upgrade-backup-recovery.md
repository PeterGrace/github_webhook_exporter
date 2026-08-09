# Upgrade, Backup, and Recovery Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver executable and tested singleton upgrade, SQLite backup, restore, and recovery procedures for the Helm deployment.

**Architecture:** Add one fail-closed Bash maintenance command that launches a hardened, digest-pinned SQLite Pod against the existing PVC, then extend the established Kind lifecycle harness to invoke the same command. Keep all maintenance tooling outside the production image and chart, and document normal versus Recreate-equivalent upgrade paths.

**Tech Stack:** Bash, kubectl, Helm 3, Kind, SQLite 3.50.4 maintenance image, Docker, Just, Rust/Cargo.

## Global Constraints

- Keep the production image minimal; do not add shell or SQLite tooling to it.
- Preserve exactly one exporter replica and one SQLite writer.
- Never copy an active database file as a backup.
- Restore only while the StatefulSet is scaled to zero and its pod is absent.
- Run restored files as UID/GID 65532 with mode `0600`.
- Never print or persist generated credentials, signatures, webhook payload material, or OTLP data in diagnostics.
- Do not add backup scheduling, online replication, point-in-time recovery, or provider-specific snapshot controllers.

---

### Task 1: Define maintenance command contracts

**Files:**
- Create: `scripts/helm-sqlite-maintenance-test.sh`
- Create: `scripts/helm-sqlite-maintenance.sh`
- Modify: `justfile`

**Interfaces:**
- Consumes: `KUBECONFIG`, optional `KUBECTL_CONTEXT`, operation `backup|restore`, namespace, StatefulSet name, PVC name, and backup basename.
- Produces: `scripts/helm-sqlite-maintenance.sh OPERATION NAMESPACE STATEFULSET PVC BACKUP_BASENAME`, plus `just helm-maintenance-unit`.

- [ ] **Step 1: Write the failing command test**

Create a fake `kubectl` on a temporary `PATH`. Assert invalid names fail before kubectl runs, backup renders a digest-pinned hardened Pod with UID/GID 65532 and SQLite `.backup`, restore refuses desired replicas `1` without creating a Pod, and restore at replicas `0` renders integrity checks, temporary restore, mode `0600`, stale WAL cleanup, and atomic replacement.

- [ ] **Step 2: Run the test to verify RED**

Run: `scripts/helm-sqlite-maintenance-test.sh`
Expected: FAIL because `scripts/helm-sqlite-maintenance.sh` does not exist or lacks the required contracts.

- [ ] **Step 3: Implement the minimal maintenance command**

Use strict mode and `umask 077`; validate all positional inputs; centralize kubectl context handling; delete stale maintenance Pods; and apply a hardened Pod manifest using:

```text
docker.io/keinos/sqlite3:3.50.4@sha256:d9e50ca08f59d96055c514175f3f4b1fcacaca97fa93508a0334c62eb9de9382
```

For `backup`, execute SQLite `.backup`, then `PRAGMA integrity_check`, and set backup mode `0600`.
For `restore`, verify desired replicas are exactly zero and the StatefulSet pod is absent before Pod creation, validate the backup, restore into a temporary database, validate it, set mode `0600`, move the current database to a pre-restore path, remove stale `-wal`/`-shm`, and replace the database.

- [ ] **Step 4: Run the focused tests and lint**

Run:

```bash
scripts/helm-sqlite-maintenance-test.sh
shellcheck scripts/helm-sqlite-maintenance.sh scripts/helm-sqlite-maintenance-test.sh
```

Expected: both commands pass without warnings.

- [ ] **Step 5: Add the Just recipe and changelog**

Add `helm-maintenance-unit` to `justfile` and a timestamped changelog entry describing the fail-closed maintenance contracts.

### Task 2: Exercise real backup, failed precondition, restore, and upgrade

**Files:**
- Modify: `scripts/helm-kind-lifecycle.sh`
- Modify: `scripts/helm-kind-lifecycle-lib-test.sh`
- Modify: `.github/workflows/helm-package-ci.yml` only if workflow contract coverage requires an explicit new recipe.

**Interfaces:**
- Consumes: Task 1's maintenance command and the existing lifecycle harness credentials, fixtures, PVC, status recorder, rollout sampler, and privacy scanner.
- Produces: recovery evidence under `KIND_ARTIFACT_DIRECTORY`, with no private values.

- [ ] **Step 1: Extend the static lifecycle contract test and verify RED**

Assert the lifecycle harness invokes online backup, tests an active-StatefulSet restore rejection,
scales to zero, waits for pod deletion, restores, verifies numeric ownership/mode, performs Helm
upgrade while stopped, scales to one, and checks post-recovery health, webhook, duplicate, queue,
and metric behavior.

Run: `scripts/helm-kind-lifecycle-lib-test.sh`
Expected: FAIL because recovery stages are absent.

- [ ] **Step 2: Add recovery helpers to the lifecycle harness**

Add helpers that invoke the maintenance command with the harness kubeconfig/context, assert the
running restore precondition fails without leaking its captured error, scale to zero and wait for
pod deletion, inspect restored file metadata in a hardened maintenance Pod, run Helm upgrade while
stopped, and restore one replica.

- [ ] **Step 3: Add post-recovery behavioral assertions**

Before backup, leave one delivery claimed and one queue attempt pending. After backup, add state
that must disappear after restore. After restore, verify readiness, repository listing, a signed
webhook using the encrypted pre-backup secret, duplicate recognition for the pre-backup delivery,
completion of the pre-backup queue attempt, and expected bounded metrics.

- [ ] **Step 4: Expand privacy evidence and diagnostics**

Record only normalized maintenance stages and numeric ownership/mode. Include maintenance Pod
descriptions/logs when available, then scan them with existing credential, signature, and forbidden
payload pattern files.

- [ ] **Step 5: Run focused static verification**

Run:

```bash
just helm-maintenance-unit
just helm-kind-lifecycle-unit
shellcheck scripts/helm-kind-lifecycle.sh scripts/helm-kind-lifecycle-lib.sh \
  scripts/helm-kind-lifecycle-lib-test.sh scripts/helm-sqlite-maintenance.sh \
  scripts/helm-sqlite-maintenance-test.sh
```

Expected: all pass.

- [ ] **Step 6: Run the real artifact acceptance test**

Run: `KIND_ARTIFACT_DIRECTORY=dist/kind-recovery just helm-kind-lifecycle`
Expected: the disposable cluster passes backup, failed precondition, restore, Recreate-equivalent upgrade, post-recovery behavior, singleton sampling, and privacy scanning.

### Task 3: Publish operator procedures

**Files:**
- Modify: `docs/operations.md`
- Modify: `charts/github-webhook-exporter/README.md`
- Create: `changelog/2026-08-09T23-55-12Z-upgrade-backup-recovery.md`

**Interfaces:**
- Consumes: the exact Task 1 command and Task 2 evidence.
- Produces: task-oriented installation/configuration links, normal and safer upgrade paths, backup, restore, troubleshooting, rollback limits, and disaster-recovery verification.

- [ ] **Step 1: Document upgrade and disruption procedures**

Describe normal one-replica `RollingUpdate`, storage-provider overlap caveats, the scale-zero/wait/upgrade/scale-one path, and why the singleton plus optional `minAvailable: 0` PDB permits downtime.

- [ ] **Step 2: Document backup and restore procedures**

Show exact maintenance command invocations. State that copying an active database file is unsupported,
that the online artifact must be copied off-PVC or protected with a platform snapshot, and that
restore requires zero replicas. Include ownership/mode verification and pre-restore artifact handling.

- [ ] **Step 3: Document recovery checks and rollback limits**

Require migration/startup success, readiness, repository decryption via signed webhook, metrics,
deduplication, and merge-queue state checks. Explain that storage-template changes and application
or schema downgrades are not guaranteed rollbacks.

- [ ] **Step 4: Run documentation and rendered-output privacy checks**

Run:

```bash
just helm-static
rg -n 'GHE_MASTER_KEY=|GHE_ADMIN_TOKEN=|Authorization: Bearer|X-Hub-Signature-256: sha256=' \
  docs/operations.md charts/github-webhook-exporter/README.md scripts/helm-sqlite-maintenance.sh
```

Expected: Helm static checks pass and the search finds no literal credential examples.

### Task 4: Final project validation and delivery

**Files:**
- Modify: timestamped changelog only if validation findings require documentation.

**Interfaces:**
- Consumes: all prior tasks.
- Produces: a validated branch ready for pull request review.

- [ ] **Step 1: Run the full mandatory gate from the top**

```bash
just fmt
cargo build --locked
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps --locked
```

Expected: every command exits zero with no warnings.

- [ ] **Step 2: Run shell and packaging gates**

```bash
mapfile -t shell_files < <(git ls-files -- '*.sh')
shellcheck "${shell_files[@]}"
just helm-static
just workflow-test
```

Expected: all pass.

- [ ] **Step 3: Review the diff and privacy surface**

Inspect `git diff --check`, changed scripts, documentation commands, rendered output, and Kind
artifacts. Confirm no secrets, signatures, payload data, temporary paths, or generated credentials
are tracked.

- [ ] **Step 4: Commit, push, and open the PR**

Use a descriptive feature commit with `Closes #48`, push the current issue branch, create a PR
against `main`, and comment the PR URL on issue #48.
