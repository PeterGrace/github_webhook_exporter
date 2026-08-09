# Production Image GHCR Publication Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Publish the validated `linux/amd64` production image to GHCR only from stable `vMAJOR.MINOR.PATCH` repository tags.

**Architecture:** Extend the existing validation workflow so pull requests, `main`, and release tags share one validation path. A dependent, tag-gated publication job validates version alignment before registry authentication, builds one image with pinned Docker actions, smoke-tests that exact local image, and then pushes only its immutable version tag.

**Tech Stack:** GitHub Actions, Docker Buildx, GHCR, Bash, Python-backed `yq` workflow contract tests, Helm metadata, Cargo metadata.

## Global Constraints

- Pull requests and `main` remain validation-only and never authenticate to or push to GHCR.
- Publication accepts only stable `vMAJOR.MINOR.PATCH` tags with no leading zeroes.
- The tag without `v`, Cargo package version, Helm chart version, and Helm appVersion must match.
- Publish only `ghcr.io/petergrace/github-webhook-exporter:<version>` for `linux/amd64`.
- Do not publish mutable `latest`, branch, SHA, or prerelease tags.
- Use only workflow-scoped `GITHUB_TOKEN`; publication permissions are `contents: read` and `packages: write`.
- Reuse the production `Dockerfile`, `scripts/container-smoke.sh`, and existing validation job.
- Pin every third-party GitHub Action to a full commit digest.

---

### Task 1: Release version validation

**Files:**
- Create: `scripts/release-version-test.sh`
- Create: `scripts/release-version.sh`
- Modify: `justfile`

**Interfaces:**
- Consumes: a Git ref/tag, `Cargo.toml`, and `charts/github-webhook-exporter/Chart.yaml`.
- Produces: `scripts/release-version.sh TAG [CARGO_TOML] [CHART_YAML]`, which prints the normalized version on success and exits nonzero with a bounded diagnostic on failure.

- [ ] **Step 1: Write the failing release-version tests**

Cover a matching `v1.2.3` fixture, malformed/prerelease/leading-zero tags, and mismatches in Cargo version, chart version, and appVersion. Use temporary fixture files and assert both exit status and normalized output without mutating repository metadata.

- [ ] **Step 2: Run the focused test and verify RED**

Run: `scripts/release-version-test.sh`

Expected: FAIL because `scripts/release-version.sh` does not exist.

- [ ] **Step 3: Implement the minimal validator**

Use strict Bash mode and this contract:

```bash
scripts/release-version.sh "${GITHUB_REF_NAME}" \
    Cargo.toml charts/github-webhook-exporter/Chart.yaml
```

Validate the stable tag with Bash regex, extract the three metadata versions without evaluating input, compare all values exactly, and print only the version without `v`.

- [ ] **Step 4: Add and run the just recipe**

Add `release-version-test` to `justfile`, run `just release-version-test`, and expect PASS.

- [ ] **Step 5: Commit**

```bash
git add scripts/release-version.sh scripts/release-version-test.sh justfile
git commit -m "test: validate release version alignment"
```

### Task 2: Tag-only GHCR publication workflow

**Files:**
- Modify: `scripts/github-actions-test.sh`
- Modify: `.github/workflows/helm-package-ci.yml`

**Interfaces:**
- Consumes: the normalized version from `scripts/release-version.sh`, existing validation outputs, and `GITHUB_TOKEN`.
- Produces: a `publish-image` job that pushes exactly one immutable GHCR image tag after validation.

- [ ] **Step 1: Extend the workflow contract test first**

Require:

- push triggers for `main` and `v*` tags;
- top-level `contents: read` only;
- `validate` and `publish-image` jobs;
- `publish-image.needs == "validate"`;
- an exact stable-tag job condition;
- job permissions of only `contents: read` and `packages: write`;
- version validation before GHCR login;
- full-SHA pins for checkout, Buildx, metadata, login, and build-push actions;
- `linux/amd64`, `load: true`, BuildKit cache, and no action-level push;
- smoke testing of the exact versioned local image before one `docker push`;
- no `latest`, SHA, or branch tag generation.

- [ ] **Step 2: Run the workflow test and verify RED**

Run: `just workflow-test`

Expected: FAIL because the workflow has no tag trigger or publication job.

- [ ] **Step 3: Implement the minimal publication job**

Extend the existing workflow trigger with `tags: ["v*"]`. Add a dependent job with:

```yaml
if: startsWith(github.ref, 'refs/tags/v')
permissions:
  contents: read
  packages: write
```

Validate and export the normalized version before authentication. Use pinned Docker Setup Buildx, Metadata, Login, and Build Push actions. Build one `linux/amd64` local image tagged only as `ghcr.io/petergrace/github-webhook-exporter:<version>`, run `scripts/container-smoke.sh` against that exact image, then push it once.

- [ ] **Step 4: Run focused workflow and release tests**

Run: `just release-version-test && just workflow-test`

Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add .github/workflows/helm-package-ci.yml scripts/github-actions-test.sh
git commit -m "ci: publish versioned production images to GHCR"
```

### Task 3: Operations documentation and changelog

**Files:**
- Modify: `docs/operations.md`
- Modify: `charts/github-webhook-exporter/README.md`
- Create: `changelog/2026-08-09T15-06-00-0400-production-image-ghcr-publication.md`

**Interfaces:**
- Consumes: the release workflow contract from Task 2.
- Produces: operator-facing release, pull, and Helm-default documentation.

- [ ] **Step 1: Add documentation assertions to the existing contract test**

Require both operator documents to name the GHCR coordinate, stable `vMAJOR.MINOR.PATCH` source tag, normalized image tag, validation-only PR/`main` behavior, immutable tag policy, and no `latest` policy.

- [ ] **Step 2: Run the workflow test and verify RED**

Run: `just workflow-test`

Expected: FAIL because the current docs still say issue #50 will supply publication.

- [ ] **Step 3: Update operations and chart documentation**

Document release tag creation, image pull/install examples, exact version alignment, tag immutability, validation-only branch behavior, and rerun guidance for a failed publication. Remove the stale issue #50 placeholder text.

- [ ] **Step 4: Add the timestamped changelog entry**

Record the tag-only publication policy, least-privilege authentication, exact-image smoke test, contract tests, and documentation changes.

- [ ] **Step 5: Run focused tests and commit**

```bash
just release-version-test
just workflow-test
git add docs/operations.md charts/github-webhook-exporter/README.md \
    changelog/2026-08-09T15-06-00-0400-production-image-ghcr-publication.md \
    scripts/github-actions-test.sh
git commit -m "docs: document immutable GHCR releases"
```

### Task 4: Full validation and delivery

**Files:**
- Verify all files changed above.

**Interfaces:**
- Consumes: completed implementation.
- Produces: a validated branch and GitHub pull request closing issue #50.

- [ ] **Step 1: Exercise focused artifacts**

Run:

```bash
just release-version-test
just workflow-test
GITHUB_REF_NAME=v0.1.0 scripts/release-version.sh v0.1.0
```

Expected: all tests pass and the validator prints `0.1.0`.

- [ ] **Step 2: Run mandatory project gates from the top**

```bash
just fmt
cargo clippy --all-targets -- -D warnings
just test
```

Expected: all pass without warnings.

- [ ] **Step 3: Run issue-specific extended gates**

```bash
cargo build --locked
cargo doc --no-deps --locked
just helm-static
just image-smoke
```

Expected: all pass; the image smoke contract validates the production artifact locally.

- [ ] **Step 4: Inspect the final diff and workflow syntax**

Run `git diff --check`, inspect `git diff origin/main...HEAD`, and use `act` only if it can safely execute without publishing credentials. Never run the tag publication job against GHCR during local validation.

- [ ] **Step 5: Commit any final validation documentation, push, open PR, and link issue**

Use a PR title `feat: publish production images to GHCR`, include actual validation evidence, and include `Closes #50`.
