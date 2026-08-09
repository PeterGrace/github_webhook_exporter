# Helm Packaging and Policy CI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add deterministic CI gates that validate every supported Helm rendering, enforce Kubernetes security policy, reject credentials, smoke-test the production image, and publish only a verified chart archive.

**Architecture:** A single render-matrix script produces named manifests consumed by independent kubeconform, Conftest, and secret-scanning scripts. Focused `just` recipes expose those checks locally and in one SHA-pinned GitHub Actions workflow; packaging re-runs the matrix against the archive before artifact upload.

**Tech Stack:** Helm 4.2.3, kubeconform 0.8.0, Conftest 0.69.0/Rego, yq 4.53.3, ShellCheck 0.11.0, just 1.58.0, Kubernetes JSON schemas, Bash, Docker, Rust 1.97.1, GitHub Actions.

## Global Constraints

- Support Kubernetes `>=1.31.0-0 <1.36.0-0`; validate built-ins against `1.31.0` and `1.35.0`.
- Never place Secret values, authorization values, OTLP header values, or credential-shaped examples in chart source, rendered output, package contents, or logs.
- Keep `secretKeyRef` names and key references valid; they are references, not embedded credentials.
- Every supported rendering must retain exactly one non-root StatefulSet replica with bounded resources and hardened security settings.
- Do not install optional CRDs, publish images/charts, or add signing, attestations, SBOMs, or cluster lifecycle tests.
- All generated artifacts go under ignored `dist/` or temporary directories.

---

### Task 1: Deterministic supported render matrix

**Files:**
- Modify: `charts/github-webhook-exporter/Chart.yaml`
- Create: `ci/helm/values/persistence.yaml`
- Create: `ci/helm/values/external-secret.yaml`
- Create: `ci/helm/values/otlp.yaml`
- Create: `ci/helm/values/pdb.yaml`
- Create: `ci/helm/values/webhook-ingress.yaml`
- Create: `ci/helm/values/metrics.yaml`
- Create: `ci/helm/values/administration.yaml`
- Create: `ci/helm/values/network-policy-default-deny.yaml`
- Create: `ci/helm/values/network-policy-bounded.yaml`
- Create: `ci/helm/render-cases.txt`
- Create: `scripts/helm-render-matrix.sh`
- Modify: `justfile`

**Interfaces:**
- Consumes: chart path and output directory as positional arguments.
- Produces: one manifest named after each line in `ci/helm/render-cases.txt`; `default` has no values file.

- [ ] **Step 1: Add the matrix contract and observe failure**

Create `ci/helm/render-cases.txt` with exactly:

```text
default
persistence
external-secret
otlp
pdb
webhook-ingress
metrics
administration
network-policy-default-deny
network-policy-bounded
```

Add a temporary `helm-render` recipe that invokes the not-yet-created script, then run:

```bash
just helm-render /tmp/gwe-render-matrix
```

Expected: FAIL because `scripts/helm-render-matrix.sh` does not exist.

- [ ] **Step 2: Add minimal values fixtures**

Use only non-sensitive values. In particular, `external-secret.yaml` changes Secret/key names and
sets all three OTLP header key references; `otlp.yaml` supplies endpoint URLs only; exposure fixtures
enable one mode each; and `network-policy-bounded.yaml` supplies explicit ingress-controller,
Prometheus, management, DNS, and OTLP selectors/ports matching the supported chart surface.

- [ ] **Step 3: Implement deterministic rendering**

Implement strict argument/tool checks, recreate the output directory, render `default` without a
values file, and render every other case with the same-named file under `ci/helm/values/`. Pass
`--kube-version 1.31.0` and reject blank/duplicate/unknown case names. After each render, require one
StatefulSet and no Secret object with yq. Print only case names, never values.

Add to `Chart.yaml`:

```yaml
kubeVersion: ">=1.31.0-0 <1.36.0-0"
```

Add recipes accepting overridable directories:

```just
helm-render output-directory="dist/rendered":
    scripts/helm-render-matrix.sh "{{helm-chart}}" "{{output-directory}}"
```

- [ ] **Step 4: Verify source matrix**

Run:

```bash
just helm-render /tmp/gwe-render-matrix
test "$(find /tmp/gwe-render-matrix -name '*.yaml' | wc -l)" -eq 10
helm template test charts/github-webhook-exporter --kube-version 1.30.0
```

Expected: first two commands PASS; final command FAILS with the chart Kubernetes version diagnostic.

- [ ] **Step 5: Commit**

```bash
git add charts/github-webhook-exporter/Chart.yaml ci/helm/values ci/helm/render-cases.txt \
  scripts/helm-render-matrix.sh justfile
git commit -m "test: add supported Helm render matrix"
```

### Task 2: Kubernetes-version-aware schema validation

**Files:**
- Create: `ci/helm/schemas/monitoring.coreos.com/servicemonitor_v1.json`
- Create: `ci/helm/schemas/README.md`
- Create: `scripts/helm-kubeconform.sh`
- Create: `ci/helm/negative/schema/unsupported-api.yaml`
- Modify: `justfile`

**Interfaces:**
- Consumes: named manifests from Task 1.
- Produces: strict kubeconform validation at Kubernetes 1.31.0 and 1.35.0, including ServiceMonitor.

- [ ] **Step 1: Add an unsupported-object negative fixture**

Create a syntactically valid `extensions/v1beta1` Ingress fixture and run kubeconform directly at
`1.31.0` to prove it fails because no supported schema exists. Then invoke the not-yet-created recipe:

```bash
just helm-kubeconform
```

Expected: FAIL because the recipe/script is absent.

- [ ] **Step 2: Vendor the ServiceMonitor schema**

Vendor `monitoring.coreos.com/servicemonitor_v1.json` from datreeio/CRDs-catalog commit
`52b0261318acc7dd0b66e032759b1f218216b980` and document repository, commit, source path, license,
and update procedure in `ci/helm/schemas/README.md`.

- [ ] **Step 3: Implement strict schema validation**

Render into a temporary directory, then run kubeconform with `-strict -summary -output pretty`, the
local schema path, and each Kubernetes version. Do not use `-ignore-missing-schemas`. After positive
validation, require the unsupported API fixture to fail and require its output to name both
`extensions/v1beta1` and `Ingress`.

Add:

```just
helm-kubeconform:
    scripts/helm-kubeconform.sh "{{helm-chart}}"
```

- [ ] **Step 4: Verify positive and negative schema paths**

Run `just helm-kubeconform`.

Expected: all ten supported renders validate at both Kubernetes versions, while the harness reports
the expected rejection of the negative fixture and exits zero.

- [ ] **Step 5: Commit**

```bash
git add ci/helm/schemas ci/helm/negative/schema scripts/helm-kubeconform.sh justfile
git commit -m "test: validate Helm manifests against Kubernetes schemas"
```

### Task 3: Rendered workload security policy

**Files:**
- Create: `ci/helm/policy/workload.rego`
- Create: `ci/helm/negative/policy/*.yaml`
- Create: `ci/helm/policy-negative-cases.txt`
- Create: `scripts/helm-policy-test.sh`
- Modify: `justfile`

**Interfaces:**
- Consumes: each YAML document as Conftest `input`.
- Produces: deny messages prefixed by stable IDs `GWE001` through `GWE012`.

- [ ] **Step 1: Add one failing fixture per rule**

Create minimal StatefulSet fixtures for: replicas two (`GWE001`), missing/root runAsNonRoot
(`GWE002`), privileged (`GWE003`), privilege escalation (`GWE004`), hostNetwork/hostPID/hostIPC
(`GWE005`), hostPath (`GWE006`), writable root filesystem (`GWE007`), capability additions
(`GWE008`), missing `drop: [ALL]` (`GWE009`), missing CPU/memory requests or limits (`GWE010`),
automounted service-account token (`GWE011`), and non-empty `serviceAccountName` (`GWE012`). Map file
names to IDs in `policy-negative-cases.txt`.

Run Conftest before adding policy:

```bash
conftest test ci/helm/negative/policy/privileged.yaml --policy ci/helm/policy
```

Expected: FAIL because no policy catches the prohibited condition (the command succeeds, violating
the harness expectation).

- [ ] **Step 2: Implement focused Rego rules**

Use `package main`, inspect only `StatefulSet` objects, and emit stable, actionable deny strings such
as:

```rego
deny contains "GWE003: containers must not be privileged" if {
    input.kind == "StatefulSet"
    some container in input.spec.template.spec.containers
    container.securityContext.privileged == true
}
```

Implement separate rules for all IDs. Treat missing required fields as violations rather than
relying on truthy defaults.

- [ ] **Step 3: Implement the policy harness**

Render the supported matrix and require `conftest test --policy ci/helm/policy` to pass for every
manifest. For each negative fixture, require failure and grep for its mapped rule ID; fail if a
fixture succeeds or reports only an unrelated rule.

Add:

```just
helm-policy:
    scripts/helm-policy-test.sh "{{helm-chart}}"
```

- [ ] **Step 4: Verify policy red-green coverage**

Run `just helm-policy`.

Expected: all supported cases pass and all twelve prohibited conditions fail with their expected
stable IDs.

- [ ] **Step 5: Commit**

```bash
git add ci/helm/policy ci/helm/negative/policy ci/helm/policy-negative-cases.txt \
  scripts/helm-policy-test.sh justfile
git commit -m "test: enforce rendered Helm workload policy"
```

### Task 4: Credential scan and packaged-chart regression checks

**Files:**
- Create: `ci/helm/negative/secrets/*.yaml`
- Create: `ci/helm/secret-negative-cases.txt`
- Create: `scripts/helm-secret-scan.sh`
- Create: `scripts/helm-package-test.sh`
- Modify: `.gitignore`
- Modify: `justfile`

**Interfaces:**
- `helm-secret-scan.sh PATH...` recursively scans files and emits category IDs `SECRET001` through
  `SECRET006` without echoing matched values.
- `helm-package-test.sh CHART DIST` produces and validates one versioned `.tgz` archive.

- [ ] **Step 1: Add negative credential fixtures**

Create one fixture each for a fixture token, literal master key, literal webhook secret, literal
Authorization header, literal OTLP header value, and Kubernetes `kind: Secret`. Map each file to its
expected category ID. Invoke the absent scanner and confirm failure.

- [ ] **Step 2: Implement category-safe scanning**

Scan text files for explicit forbidden markers and credential-shaped assignments. Report only file,
line number, and category ID; never print the matching line. Allow `secretKeyRef`, Secret names, and
key names. Add a self-test mode used by the harness: every negative fixture must fail with exactly
its expected category, while chart source, values fixtures, and rendered manifests must pass.

Add:

```just
helm-secrets:
    scripts/helm-secret-scan.sh --test "{{helm-chart}}"
```

- [ ] **Step 3: Write a failing package contract**

Invoke `just helm-package` before the recipe exists. Expected: FAIL. Then implement packaging to:

1. clean and recreate `dist/`;
2. run `helm package`;
3. require exactly `github-webhook-exporter-0.1.0.tgz`;
4. exercise `helm show chart` and `helm show values`;
5. extract the archive safely into a temporary directory;
6. rerun the full render matrix against the extracted chart;
7. run schema, policy, and secret checks against archive-derived output; and
8. ensure the archive contains no generated files or negative fixtures.

- [ ] **Step 4: Add package and aggregate recipes**

```just
helm-package output-directory="dist":
    scripts/helm-package-test.sh "{{helm-chart}}" "{{output-directory}}"

helm-static: helm-lint helm-test helm-kubeconform helm-policy helm-secrets helm-package
```

Ignore `/dist/`, run `just helm-static`, then run:

```bash
helm show chart dist/github-webhook-exporter-0.1.0.tgz
helm template archive dist/github-webhook-exporter-0.1.0.tgz --kube-version 1.31.0 >/dev/null
```

Expected: all commands PASS.

- [ ] **Step 5: Commit**

```bash
git add .gitignore ci/helm/negative/secrets ci/helm/secret-negative-cases.txt \
  scripts/helm-secret-scan.sh scripts/helm-package-test.sh justfile
git commit -m "test: scan and validate packaged Helm charts"
```

### Task 5: Pinned CI tool installation and GitHub Actions workflow

**Files:**
- Create: `ci/tool-versions.env`
- Create: `scripts/install-ci-tools.sh`
- Create: `scripts/github-actions-test.sh`
- Create: `.github/workflows/helm-package-ci.yml`
- Modify: `justfile`

**Interfaces:**
- Installer consumes `ci/tool-versions.env` and installs into an explicit writable directory.
- Workflow test verifies trigger, SHA pins, command order, artifact path, and absence of mutable tags.

- [ ] **Step 1: Write the failing workflow contract**

Implement `github-actions-test.sh` to require:

- pull request and `main` push triggers;
- `actions/checkout` SHA `11d5960a326750d5838078e36cf38b85af677262`;
- `actions/upload-artifact` SHA `ea165f8d65b6e75b540449e92b4886f43607fa02`;
- pinned installer execution before tool use;
- `just helm-static`, `just image-smoke`, standard project gates, then upload in that order; and
- artifact path `dist/github-webhook-exporter-0.1.0.tgz`.

Run it before creating the workflow. Expected: FAIL because the workflow is absent.

- [ ] **Step 2: Add exact versions and checksums**

Populate `ci/tool-versions.env` with:

```text
HELM_VERSION=4.2.3
HELM_SHA256=e9b88b4ee95b18c706839c28d3a0220e5bc470e9cd9262410c90793c45ff8b7c
KUBECONFORM_VERSION=0.8.0
KUBECONFORM_SHA256=9bc2bffbf71f261128533edaf912153948b7ff238f9a531ae6d34466ec287883
CONFTEST_VERSION=0.69.0
CONFTEST_SHA256=96fc2fbf11f0afde51256647127e6f00a64ce839a4d9a0a1aef2426c0e6f4b3f
YQ_VERSION=4.53.3
YQ_SHA256=fa52a4e758c63d38299163fbdd1edfb4c4963247918bf9c1c5d31d84789eded4
SHELLCHECK_VERSION=0.11.0
SHELLCHECK_SHA256=8c3be12b05d5c177a04c29e3c78ce89ac86f1595681cab149b65b97c4e227198
JUST_VERSION=1.58.0
JUST_SHA256=4a5cc2f53e6f0f8c59092a6cc38291eb729d46a7dd95d3ae582008881b84931d
RUST_VERSION=1.97.1
```

- [ ] **Step 3: Implement verified installation**

Download architecture-specific release artifacts from immutable version URLs, verify each with
`sha256sum --check`, extract only expected binaries, and install into the caller-provided directory.
Reject unsupported architectures. Install Rust 1.97.1 through rustup with rustfmt and clippy. Print
only installed version output.

- [ ] **Step 4: Add the SHA-pinned workflow**

Use `ubuntu-24.04`, least-privilege `contents: read`, a concurrency group that cancels stale branch
runs, and a single sequential job. Set `CONTAINER_IMAGE=github-webhook-exporter:ci`. Install tools,
append their directory to `GITHUB_PATH`, run ShellCheck over tracked shell files, execute
`just helm-static`, `just image-smoke`, `just fmt`, `cargo build --locked`,
`cargo clippy --all-targets -- -D warnings`, `just test`, and `cargo doc --no-deps --locked`, then
upload the exact archive with `if-no-files-found: error` and a fixed retention period.

Add:

```just
workflow-test:
    scripts/github-actions-test.sh .github/workflows/helm-package-ci.yml
```

- [ ] **Step 5: Verify workflow and installer contracts**

Run:

```bash
just workflow-test
shellcheck scripts/*.sh
scripts/install-ci-tools.sh /tmp/gwe-ci-tools
/tmp/gwe-ci-tools/helm version --short
/tmp/gwe-ci-tools/kubeconform -v
/tmp/gwe-ci-tools/conftest --version
```

Expected: all commands PASS and report the pinned versions.

- [ ] **Step 6: Commit**

```bash
git add ci/tool-versions.env scripts/install-ci-tools.sh scripts/github-actions-test.sh \
  .github/workflows/helm-package-ci.yml justfile
git commit -m "ci: validate Helm package and production image"
```

### Task 6: Documentation, changelog, and complete validation

**Files:**
- Modify: `charts/github-webhook-exporter/README.md`
- Modify: `docs/operations.md`
- Create: `changelog/2026-08-09T12-18-55Z-helm-packaging-policy-ci.md`

**Interfaces:**
- Consumes: all new recipes and CI behavior.
- Produces: local contributor commands, pinned-tool update guidance, artifact expectations, and issue changelog.

- [ ] **Step 1: Add documentation assertions**

Extend `github-actions-test.sh` to require README/operations references to `just helm-static`,
`just image-smoke`, the supported Kubernetes range, `dist/github-webhook-exporter-0.1.0.tgz`, and
the distinction between static checks and later cluster acceptance. Run the test and observe failure.

- [ ] **Step 2: Document operation and maintenance**

Document prerequisites, focused recipes, the bounded matrix, policy/secret diagnostics, local
package inspection, tool/checksum updates, and the fact that passing static checks does not prove
cluster lifecycle behavior. Add the required timestamped changelog entry.

- [ ] **Step 3: Run artifact-specific validation**

```bash
command -v docker
command -v helm
command -v kubeconform
command -v conftest
just workflow-test
just helm-static
just image-smoke
helm show chart dist/github-webhook-exporter-0.1.0.tgz
helm template archive dist/github-webhook-exporter-0.1.0.tgz --kube-version 1.35.0 >/dev/null
```

Expected: every command exits zero; the image smoke test confirms runtime and persistence contracts.

- [ ] **Step 4: Run standard project gates from the top**

```bash
just fmt
cargo build --locked
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps --locked
```

Expected: every command exits zero without warnings. If any command fails, fix it and rerun this
entire sequence.

- [ ] **Step 5: Review scope and commit**

```bash
git diff --check
git status --short
git diff origin/main...HEAD --stat
git add charts/github-webhook-exporter/README.md docs/operations.md changelog \
  scripts/github-actions-test.sh
git commit -m "docs: explain Helm package validation"
```

- [ ] **Step 6: Deliver issue #46**

Push `feat-issue-46-helm-packaging-policy-ci`, open a PR titled
`feat: validate Helm packaging and policy in CI`, include actual validation results, add
`Closes #46`, and comment the PR URL on issue #46.
