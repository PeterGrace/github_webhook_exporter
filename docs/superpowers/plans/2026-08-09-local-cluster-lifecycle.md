# Local Cluster Lifecycle Acceptance Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a reproducible Kind acceptance suite proving the packaged image and Helm chart preserve application state, isolate collector failures, reject broken storage readiness, shut down gracefully, and maintain one SQLite writer.

**Architecture:** Keep Kubernetes orchestration in strict Bash, split deterministic cryptographic/artifact assertions into a sourceable helper library, and exercise the real production image and chart in a disposable Kind cluster. Runtime secrets are file-backed and private; diagnostics are collected on every failure and scanned before publication.

**Tech Stack:** Bash, Kind, Docker, Helm, kubectl, curl, jq, OpenSSL, GitHub Actions.

## Global Constraints

- Never commit, print, or pass generated credential values through process arguments.
- Preserve diagnostics without masking the original test failure.
- Use fixed polling deadlines and normalized failure messages.
- The test supports one application replica and `linux/amd64` only.
- No cloud-provider, ingress, certificate, load, backup, or high-availability testing.
- Every code iteration receives a timestamped file under `changelog/`.

---

### Task 1: Deterministic lifecycle helper contracts

**Files:**
- Create: `scripts/helm-kind-lifecycle-lib.sh`
- Create: `scripts/helm-kind-lifecycle-lib-test.sh`
- Modify: `justfile`
- Create: `changelog/<timestamp>-kind-lifecycle-helper-contracts.md`

**Interfaces:**
- Produces: `require_command NAME`, `fail MESSAGE`, `assert_equal EXPECTED ACTUAL CONTRACT`, `hmac_sha256 SECRET_FILE PAYLOAD_FILE`, `record_http_status OUTPUT_FILE LABEL STATUS`, and `scan_private_artifacts ARTIFACT_DIRECTORY SECRET_FILE...`.
- `hmac_sha256` returns only the `sha256=<hex>` signature needed for the HTTP header.
- `scan_private_artifacts` reports only a category and path, never a matched value.

- [ ] **Step 1: Write the failing helper test**

Create an executable test that sources the not-yet-existing library, signs the literal payload
`{"action":"opened"}` from mode-600 temporary files, compares against the independently calculated
literal HMAC, records a normalized `repository_create=201` status, and proves artifact scanning both
accepts clean files and rejects a file containing a generated sentinel without echoing that sentinel.

```bash
expected='sha256=2727dff643281f914f6ff3d4623ff61a24521d1fa3f23a17d2402eae9471e44a'
actual="$(hmac_sha256 "${secret_file}" "${payload_file}")"
assert_equal "${expected}" "${actual}" 'HMAC-SHA256 signature'
```

- [ ] **Step 2: Run the test and verify RED**

Run: `scripts/helm-kind-lifecycle-lib-test.sh`
Expected: nonzero because `scripts/helm-kind-lifecycle-lib.sh` does not exist.

- [ ] **Step 3: Implement the minimal helper library**

Use OpenSSL file input and byte-preserving command substitution. Scan each file with
`grep --fixed-strings --files-with-matches --file=<secret-file>` while redirecting match output;
print only the artifact path after a match. Validate every secret file is non-empty before scanning.

- [ ] **Step 4: Run the helper test and lint it**

Run:

```bash
scripts/helm-kind-lifecycle-lib-test.sh
shellcheck scripts/helm-kind-lifecycle-lib.sh scripts/helm-kind-lifecycle-lib-test.sh
```

Expected: both commands pass without output containing the sentinel.

- [ ] **Step 5: Add a focused Just recipe and changelog, then commit**

Add `helm-kind-lifecycle-unit` to run the helper test.

```bash
git add scripts/helm-kind-lifecycle-lib.sh scripts/helm-kind-lifecycle-lib-test.sh justfile changelog/
git commit -m "test: define Kind lifecycle helper contracts"
```

### Task 2: Happy-path cluster, webhooks, and restart persistence

**Files:**
- Create: `scripts/helm-kind-lifecycle.sh`
- Create: `ci/kind/pull-request-enqueued.json`
- Create: `ci/kind/pull-request-dequeued.json`
- Create: `ci/kind/merge-group-checks-requested.json`
- Create: `ci/kind/merge-group-destroyed.json`
- Modify: `justfile`
- Create: `changelog/<timestamp>-kind-lifecycle-persistence.md`

**Interfaces:**
- Command: `scripts/helm-kind-lifecycle.sh CHART_DIRECTORY IMAGE ARTIFACT_DIRECTORY`.
- Environment: `KEEP_KIND_CLUSTER=true` optionally preserves the cluster for investigation; default cleanup deletes it.
- Produces: a private artifact directory containing statuses, rendered objects, descriptions, events, and redacted logs.

- [ ] **Step 1: Add the failing Just integration contract**

Add `helm-kind-lifecycle` so it invokes the missing executable with
`charts/github-webhook-exporter`, `{{container-image}}`, and
`${KIND_ARTIFACT_DIRECTORY:-dist/kind-lifecycle}` after `image-build`.

Run: `KIND_ARTIFACT_DIRECTORY="$(mktemp -d)" just helm-kind-lifecycle`
Expected: nonzero because the lifecycle script is missing.

- [ ] **Step 2: Add hand-checked webhook fixtures**

Use repository `acceptance/repository`, pull request 42, a valid 40-character SHA, fixed RFC3339
timestamps two minutes apart, and only fields consumed by the application. Merge-group fixtures
must open and complete a durable attempt around the pod restart. Fixtures contain no generated
credentials.

- [ ] **Step 3: Implement owned cluster setup and cleanup**

The script must:

```text
validate tools and inputs -> create private temp/artifact directories ->
build collision-resistant cluster name -> create Kind cluster -> kind load docker-image ->
create namespace -> create Secret from mode-600 files ->
helm install --wait --rollback-on-failure
```

Use a dedicated kubeconfig. The EXIT trap stores the original status, captures diagnostics when the
cluster exists, removes secret files, and deletes only the generated cluster. Do not use `set -x`.

- [ ] **Step 4: Implement application access and signed webhook helpers**

Start one managed `kubectl port-forward service/... 18080:8080`, wait for it, and expose functions
that call probes, administration API, metrics, and `/webhooks/github`. Build JSON repository
creation with `jq --arg` reading the webhook secret from its file; send request bodies from files;
calculate signatures via Task 1. Record only labels and statuses.

- [ ] **Step 5: Exercise happy-path and persistence behavior**

Assert:

- live, ready, and metrics return 200;
- repository creation returns 201;
- signed pull-request enqueue and merge-group open return 204;
- bounded webhook and repository metrics are present;
- deleting pod `github-webhook-exporter-0` yields a different UID that becomes Ready;
- listing repositories after restart returns the configured repository;
- replaying the original delivery returns 204 and increments the duplicate outcome;
- post-restart dequeue/destroy transitions complete the durable queue attempts and expose expected bounded outcomes.

Use unique UUID-shaped delivery IDs stored as non-secret constants.

- [ ] **Step 6: Configure and verify unavailable-collector isolation**

Install with an unroutable in-cluster OTLP endpoint and short exporter timeout. Generate accepted
activity, poll metrics for a normalized transport/timeout failure, and verify live, ready, and a new
signed webhook remain successful. Assert logs do not contain URLs, request payload fragments, or
credential values.

- [ ] **Step 7: Run real integration GREEN**

Run:

```bash
KIND_ARTIFACT_DIRECTORY="$(mktemp -d)" just helm-kind-lifecycle
```

Expected: production image builds, one disposable cluster passes, and cleanup removes the cluster.

- [ ] **Step 8: Add changelog and commit**

```bash
git add scripts/helm-kind-lifecycle.sh ci/kind justfile changelog/
git commit -m "test: verify Kind lifecycle persistence"
```

### Task 3: Broken readiness, graceful shutdown, singleton rollout, and diagnostics

**Files:**
- Modify: `scripts/helm-kind-lifecycle.sh`
- Modify: `scripts/helm-kind-lifecycle-lib-test.sh`
- Create: `changelog/<timestamp>-kind-lifecycle-failure-contracts.md`

**Interfaces:**
- Extends the Task 2 command without changing its arguments.
- Artifact contract adds `objects.yaml`, `events.txt`, `statefulset.txt`, `pods.txt`, `logs-current.txt`, `logs-previous.txt`, `http-statuses.txt`, and `rollout-samples.txt` when available.

- [ ] **Step 1: Write failing privacy/diagnostic helper cases**

Extend the helper test with multiple secret files, nested artifact directories, binary-safe grep
handling, and a forbidden payload fragment. Confirm the scanner fails with the artifact relative
path but neither secret nor payload text.

Run: `just helm-kind-lifecycle-unit`
Expected: fail because multiple forbidden-pattern classes are not yet supported.

- [ ] **Step 2: Generalize the scanner and verify GREEN**

Add a category-aware scanner input format without evaluating values. Re-run the helper test and
ShellCheck.

- [ ] **Step 3: Add the broken-database readiness case**

After preserving the primary StatefulSet assertions, create an isolated pod from the rendered
container configuration with `GHE_DATABASE_PATH=/proc/github-webhook-exporter.db`, the same
Secret refs, and no readiness bypass. Assert within a fixed deadline that Ready never becomes True,
the container exits or remains unready, and no successful readiness response is recorded. Capture
its description and logs, then delete it.

- [ ] **Step 4: Add controlled SIGTERM acceptance**

Start bounded background webhook submissions, record the pod UID and monotonic start time, delete
the pod with the chart grace period, and assert the old UID disappears and a replacement becomes
Ready before that deadline. Verify the rendered pod has no `lifecycle.preStop`, application and
telemetry timeout sum is below pod grace, and previous logs contain normalized shutdown stages.
Stop and reap every background process in cleanup.

- [ ] **Step 5: Add singleton rollout sampling**

Change a harmless ConfigMap-backed chart value with `helm upgrade`, then sample pods and container
statuses until rollout completion. For every Running exporter container, inspect its pod volume
claim reference; append timestamp, UID, and container ID to `rollout-samples.txt`. Track the maximum
simultaneous running containers referencing the target PVC and assert the observed maximum equals
one. This is an observation of Kubernetes status during the rollout, not proof that sub-sample
process overlap is impossible.

- [ ] **Step 6: Capture and scan diagnostics**

Make diagnostic commands best-effort and value-free. Scan all artifacts for generated master key,
admin token, webhook secret, signatures, authorization values, raw payload-only sentinel fields,
and database path internals. A scan failure must retain artifacts and return nonzero without
printing matched content.

- [ ] **Step 7: Run full lifecycle integration**

Run the Task 2 integration command again. Expected: all positive and negative lifecycle contracts
pass, artifacts scan clean, and the cluster is removed.

- [ ] **Step 8: Add changelog and commit**

```bash
git add scripts/helm-kind-lifecycle.sh scripts/helm-kind-lifecycle-lib-test.sh changelog/
git commit -m "test: cover Kubernetes failure and rollout lifecycle"
```

### Task 4: Pinned tooling, CI diagnostics, and operator documentation

**Files:**
- Modify: `ci/tool-versions.env`
- Modify: `scripts/install-ci-tools.sh`
- Modify: `scripts/github-actions-test.sh`
- Modify: `.github/workflows/helm-package-ci.yml`
- Modify: `docs/operations.md`
- Modify: `charts/github-webhook-exporter/README.md`
- Create: `changelog/<timestamp>-kind-lifecycle-ci.md`

**Interfaces:**
- Installer adds checksum-verified Kind and kubectl binaries required by the workflow.
- Workflow runs `just helm-kind-lifecycle` and always uploads `dist/kind-lifecycle`.

- [ ] **Step 1: Extend the failing workflow contract test**

Require pinned Kind/kubectl variables, a lifecycle step after image/static validation, and a pinned
`actions/upload-artifact` step with:

```yaml
if: always()
with:
  name: kind-lifecycle-diagnostics
  path: dist/kind-lifecycle
  if-no-files-found: warn
  retention-days: 14
```

Run: `just workflow-test`
Expected: fail because installer/workflow fields are absent.

- [ ] **Step 2: Add pinned installers and workflow execution**

Add exact Linux amd64 versions and SHA-256 checksums to `ci/tool-versions.env`; follow the existing
download, checksum, private staging, and atomic install conventions. Set
`KIND_ARTIFACT_DIRECTORY=dist/kind-lifecycle` in CI and invoke the lifecycle recipe before standard
Rust gates. Keep action references full-SHA pinned.

- [ ] **Step 3: Verify workflow GREEN**

Run:

```bash
just workflow-test
scripts/install-ci-tools.sh "$(mktemp -d)"
```

Expected: workflow contract passes and every tool installs from a verified archive/binary.

- [ ] **Step 4: Document exact local and CI behavior**

Document prerequisites, command, artifact path, privacy guarantees, cleanup/preservation option,
contracts covered, and how the test differs from static Helm validation. Do not include sample
credentials.

- [ ] **Step 5: Add changelog and commit**

```bash
git add ci/tool-versions.env scripts/install-ci-tools.sh scripts/github-actions-test.sh \
  .github/workflows/helm-package-ci.yml docs/operations.md \
  charts/github-webhook-exporter/README.md changelog/
git commit -m "ci: exercise Helm lifecycle in Kind"
```

### Task 5: Full verification and delivery

**Files:**
- Modify only files required by verification fixes.
- Create: `changelog/<timestamp>-issue-47-final-validation.md`

- [ ] **Step 1: Run artifact-specific gates**

```bash
just helm-kind-lifecycle-unit
shellcheck $(git ls-files -- '*.sh')
KIND_ARTIFACT_DIRECTORY="$(mktemp -d)" just helm-kind-lifecycle
```

Expected: all pass with a clean artifact scan and no retained Kind cluster.

- [ ] **Step 2: Run the complete project gates in order**

```bash
just fmt
cargo build
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps
```

Expected: every command exits zero without warnings.

- [ ] **Step 3: Inspect final state**

```bash
git diff --check
git status --short
git log --oneline origin/main..HEAD
```

Confirm only issue #47 changes are present, no generated credentials or build artifacts are tracked,
and all public Rust items remain documented.

- [ ] **Step 4: Commit final validation record**

```bash
git add changelog/ scripts/ docs/ ci/ .github/ justfile
git commit -m "docs: record issue 47 validation"
```

- [ ] **Step 5: Push and open the pull request**

Push `github-webhook-exporter/gwe-47`, open a PR against `main` titled
`feat: test lifecycle and persistence in a local cluster`, include exact validation evidence and
`Closes #47`, then comment on issue #47 with the PR URL.
