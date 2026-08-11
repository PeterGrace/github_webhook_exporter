# CI Build Caching Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reduce pull-request and release CI time by caching Rust dependency builds, building the validation image once, and limiting cache-disabled reproducibility builds to trusted pushes.

**Architecture:** cargo-chef converts container dependency compilation into an exportable BuildKit layer, while GitHub Actions caches auxiliary CI tools, host Cargo artifacts, and BuildKit layers independently. Pull requests load one cached validation image for smoke and Kind checks; `main` and tag pushes additionally retain the two-build, cache-disabled reproducibility gate.

**Tech Stack:** GitHub Actions, Docker Buildx/BuildKit, cargo-chef 0.1.71, Cargo, Just, Bash, Python workflow contract tests

## Global Constraints

- Keep all third-party GitHub Actions pinned to immutable commit SHAs.
- Keep the Rust builder image pinned to its existing SHA-256 digest and Rust 1.97.1.
- Support only `linux/amd64`; no cache fallback may cross operating systems or architectures.
- Preserve the distroless runtime, UID/GID `65532:65532`, direct entrypoint, and reproducible timestamps.
- Preserve two `--no-cache` image builds on pushes to `main` and semantic-version tags.
- Pull requests must perform exactly one production image build and no reproducibility build.
- Preserve release permissions, authentication boundaries, immutable metadata, and publication behavior.
- Cache misses must run full verified builds and must never skip validation commands.
- Do not inject a host-built binary into the production image.

## File Structure

- `Dockerfile`: plan, cook, and build Rust dependencies/application in separately cacheable stages.
- `justfile`: expose checks that consume an already-loaded image while preserving build-and-check local recipes.
- `.github/workflows/helm-package-ci.yml`: restore caches, build/load one validation image, condition the reproducibility gate, and share the BuildKit scope with publication.
- `scripts/github-actions-test.sh`: enforce Dockerfile, Just recipe, workflow ordering, cache, and release-security contracts.
- `docs/operations.md`: explain the PR versus trusted-push build and reproducibility policy.
- `changelog/2026-08-11T00-43-23Z-ci-build-caching-implementation.md`: record the implemented CI optimization.

---

### Task 1: Make Rust dependency compilation an exportable Docker layer

**Files:**
- Modify: `scripts/github-actions-test.sh:25-80`
- Modify: `Dockerfile:1-30`

**Interfaces:**
- Consumes: `Cargo.toml`, `Cargo.lock`, `.cargo/config.toml`, `migrations/**`, and `src/**` from the existing Docker build context.
- Produces: cargo-chef planner output at `/build/recipe.json`, a cooked release dependency layer, and the unchanged `/out/usr/local/bin/github_webhook_exporter` runtime artifact.

- [ ] **Step 1: Add failing Dockerfile contract assertions**

After `require_fragment` is defined in `scripts/github-actions-test.sh`, add exact structural checks:

```python
with open("Dockerfile", encoding="utf-8") as file_handle:
    dockerfile = file_handle.read()

required_dockerfile_fragments = (
    "cargo install cargo-chef --version 0.1.71 --locked",
    "FROM chef AS planner",
    "cargo chef prepare --recipe-path recipe.json",
    "FROM chef AS builder",
    "COPY --from=planner /build/recipe.json recipe.json",
    "cargo chef cook --locked --release --recipe-path recipe.json",
    "cargo build --locked --release",
)
for fragment in required_dockerfile_fragments:
    if fragment not in dockerfile:
        fail(f"Dockerfile is missing cache contract: {fragment}")

if dockerfile.count("cargo build --locked --release") != 1:
    fail("Dockerfile must compile the application exactly once")
if dockerfile.index("cargo chef cook --locked --release") > dockerfile.rindex(
    "COPY migrations/ migrations/"
):
    fail("Dockerfile must cook dependencies before copying application inputs")
if dockerfile.index("cargo build --locked --release") > dockerfile.index(
    "ARG SOURCE_DATE_EPOCH=0"
):
    fail("SOURCE_DATE_EPOCH must not invalidate application compilation")
```

- [ ] **Step 2: Run the contract test and verify it fails**

Run:

```bash
just workflow-test
```

Expected: FAIL with `Dockerfile is missing cache contract: cargo install cargo-chef --version 0.1.71 --locked`.

- [ ] **Step 3: Replace the builder with planner, dependency, and application layers**

Replace `Dockerfile` with this structure, retaining the existing image digests verbatim:

```dockerfile
# syntax=docker/dockerfile:1.7

FROM --platform=linux/amd64 docker.io/library/rust:1.97.1-bookworm@sha256:e544a8ee0b93bb2ddc8c67a80606f040998eff3847e4deed988d0874559f52a8 AS chef

WORKDIR /build
RUN --mount=type=cache,id=ghe-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    cargo install cargo-chef --version 0.1.71 --locked

FROM chef AS planner
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ .cargo/
COPY migrations/ migrations/
COPY src/ src/
RUN cargo chef prepare --recipe-path recipe.json

FROM chef AS builder
COPY --from=planner /build/recipe.json recipe.json
RUN --mount=type=cache,id=ghe-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=ghe-target,target=/build/target,sharing=locked \
    cargo chef cook --locked --release --recipe-path recipe.json
COPY Cargo.toml Cargo.lock ./
COPY .cargo/ .cargo/
COPY migrations/ migrations/
COPY src/ src/
RUN --mount=type=cache,id=ghe-cargo-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=ghe-target,target=/build/target,sharing=locked \
    cargo build --locked --release

ARG SOURCE_DATE_EPOCH=0
RUN install -D -m 0555 \
        target/release/github_webhook_exporter \
        /out/usr/local/bin/github_webhook_exporter \
    && install -d -m 0700 -o 65532 -g 65532 \
        /out/var/lib/github-webhook-exporter \
    && find /out -exec \
        touch --no-dereference --date="@${SOURCE_DATE_EPOCH}" -- {} +

FROM --platform=linux/amd64 gcr.io/distroless/cc-debian12:nonroot@sha256:471dbca9cad607b9a32c10e9c31fb09ffaeb2d460e0afbff86c27abbc80b1b98

COPY --from=builder /out/ /
WORKDIR /var/lib/github-webhook-exporter
USER 65532:65532
EXPOSE 8080/tcp
ENTRYPOINT ["/usr/local/bin/github_webhook_exporter"]
```

The `SOURCE_DATE_EPOCH` declaration deliberately follows compilation so release metadata changes invalidate only assembly layers.

- [ ] **Step 4: Run contract and production-image tests**

Run:

```bash
just workflow-test
just image-smoke
just image-reproducibility-test
```

Expected: all three commands exit zero; the reproducibility script reports matching image IDs.

- [ ] **Step 5: Commit the Docker cache layer**

```bash
git add Dockerfile scripts/github-actions-test.sh
git commit -m "build: cache container Rust dependencies"
```

---

### Task 2: Reuse one loaded image across validation checks

**Files:**
- Modify: `scripts/github-actions-test.sh:70-100`
- Modify: `justfile:35-70`

**Interfaces:**
- Consumes: `CONTAINER_IMAGE`, defaulting to `github-webhook-exporter:dev`, and `KIND_ARTIFACT_DIRECTORY`, defaulting to `dist/kind-lifecycle`.
- Produces: `image-smoke-loaded` and `helm-kind-lifecycle-loaded` Just recipes that never invoke `image-build`.

- [ ] **Step 1: Add failing Just recipe contract assertions**

Add these assertions to `scripts/github-actions-test.sh` after the Dockerfile checks:

```python
with open("justfile", encoding="utf-8") as file_handle:
    justfile = file_handle.read()

required_just_fragments = (
    "image-smoke: image-build image-smoke-loaded",
    'image-smoke-loaded:\n    scripts/container-smoke.sh "{{container-image}}"',
    "helm-kind-lifecycle: image-build helm-kind-lifecycle-loaded",
    'helm-kind-lifecycle-loaded:\n    scripts/helm-kind-lifecycle.sh "{{helm-chart}}" "{{container-image}}"',
)
for fragment in required_just_fragments:
    if fragment not in justfile:
        fail(f"justfile is missing loaded-image contract: {fragment}")
```

- [ ] **Step 2: Run the contract test and verify it fails**

Run:

```bash
just workflow-test
```

Expected: FAIL with `justfile is missing loaded-image contract: image-smoke: image-build image-smoke-loaded`.

- [ ] **Step 3: Split build-and-check recipes from loaded-image checks**

Replace the existing Kind lifecycle and image smoke recipes in `justfile` with:

```just
# Exercise lifecycle and persistence after building the production image.
helm-kind-lifecycle: image-build helm-kind-lifecycle-loaded

# Exercise lifecycle and persistence using an image already loaded into Docker.
helm-kind-lifecycle-loaded:
    scripts/helm-kind-lifecycle.sh "{{helm-chart}}" "{{container-image}}" \
        "${KIND_ARTIFACT_DIRECTORY:-dist/kind-lifecycle}"

# Build the supported linux/amd64 production image.
image-build:
    docker build --platform linux/amd64 --tag "{{container-image}}" .

# Build and exercise the production image contracts.
image-smoke: image-build image-smoke-loaded

# Exercise production image contracts using an image already loaded into Docker.
image-smoke-loaded:
    scripts/container-smoke.sh "{{container-image}}"
```

Keep `image-reproducibility-test` unchanged.

- [ ] **Step 4: Verify recipe parsing and both execution paths**

Run:

```bash
just --list
just workflow-test
just image-smoke
CONTAINER_IMAGE=github-webhook-exporter:dev just image-smoke-loaded
```

Expected: `just --list` includes both loaded-image recipes, and all three test commands exit zero without the loaded-only command invoking `docker build`.

- [ ] **Step 5: Commit loaded-image recipe support**

```bash
git add justfile scripts/github-actions-test.sh
git commit -m "build: reuse loaded images in validation checks"
```

---

### Task 3: Add GitHub caches and trusted-push reproducibility gating

**Files:**
- Modify: `scripts/github-actions-test.sh:190-390`
- Modify: `.github/workflows/helm-package-ci.yml:20-185`

**Interfaces:**
- Consumes: `RUSTUP_TOOLCHAIN=1.97.1`, `ci/tool-versions.env`, `scripts/install-ci-tools.sh`, GitHub cache service, and the loaded-image Just recipes from Task 2.
- Produces: auxiliary tool cache key `ci-tools-${{ runner.os }}-${{ runner.arch }}-${{ hashFiles('ci/tool-versions.env', 'scripts/install-ci-tools.sh') }}` and BuildKit scope `production-image-linux-amd64`.

- [ ] **Step 1: Rewrite the expected validation workflow contract first**

In `expected_validate_steps`, replace the setup and image/build entries with exact contracts equivalent to:

```python
{"uses": "actions/checkout@11d5960a326750d5838078e36cf38b85af677262"},
{
    "run": "rustup -q toolchain install \"$RUSTUP_TOOLCHAIN\" --profile minimal \\\n    --component rustfmt --component clippy --no-self-update\n",
},
{
    "id": "ci-tools-cache",
    "uses": "actions/cache@0057852bfaa89a56745cba8c7296529d2fc39830",
    "with": {
        "path": "${{ runner.temp }}/ci-tools",
        "key": "ci-tools-${{ runner.os }}-${{ runner.arch }}-${{ hashFiles('ci/tool-versions.env', 'scripts/install-ci-tools.sh') }}",
    },
},
{
    "if": "steps.ci-tools-cache.outputs.cache-hit != 'true'",
    "run": 'scripts/install-ci-tools.sh "$RUNNER_TEMP/ci-tools"',
},
{"run": 'echo "$RUNNER_TEMP/ci-tools" >> "$GITHUB_PATH"'},
{"uses": "Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6"},
```

Replace the old image smoke, reproducibility, Kind, and standalone Cargo build entries with:

```python
{"uses": "docker/setup-buildx-action@8d2750c68a42422c14e847fe6c8ac0403b4cbd6f"},
{
    "uses": "docker/build-push-action@10e90e3645eae34f1e60eeb005ba3a3d33f178e8",
    "with": {
        "context": ".",
        "platforms": "linux/amd64",
        "load": True,
        "push": False,
        "provenance": False,
        "tags": "github-webhook-exporter:ci",
        "cache-from": "type=gha,scope=production-image-linux-amd64",
        "cache-to": "type=gha,mode=max,scope=production-image-linux-amd64",
    },
},
{"run": "just image-smoke-loaded"},
{"if": "github.event_name == 'push'", "run": "just image-reproducibility-test"},
{"run": "just helm-maintenance-unit"},
{"run": "just helm-kind-lifecycle-loaded"},
{"run": "just fmt"},
{"run": "cargo clippy --all-targets -- -D warnings"},
{"run": "just test"},
{"run": "cargo doc --no-deps --locked"},
```

Keep Helm static checks before Buildx, artifact uploads after Rust checks, and every existing security assertion. Change both release cache values from `scope=production-image` to `scope=production-image-linux-amd64`. Update the workflow mutation fixture's expected validation step number from `5` to `7` because `just workflow-test` becomes the seventh step.

- [ ] **Step 2: Run the workflow contract and verify it fails**

Run:

```bash
just workflow-test
```

Expected: FAIL with `workflow must contain the expected validation steps only` because the current workflow lacks the new setup, cache, Buildx, and loaded-image steps.

- [ ] **Step 3: Implement cached setup and one validation image build**

In `.github/workflows/helm-package-ci.yml`, make the validation sequence match the contracts from Step 1. The new setup and image sections must be:

```yaml
      - name: Install pinned Rust toolchain
        run: |
          rustup -q toolchain install "$RUSTUP_TOOLCHAIN" --profile minimal \
              --component rustfmt --component clippy --no-self-update

      - name: Cache pinned CI tools
        id: ci-tools-cache
        uses: actions/cache@0057852bfaa89a56745cba8c7296529d2fc39830
        with:
          path: ${{ runner.temp }}/ci-tools
          key: ci-tools-${{ runner.os }}-${{ runner.arch }}-${{ hashFiles('ci/tool-versions.env', 'scripts/install-ci-tools.sh') }}

      - name: Install pinned CI tools
        if: steps.ci-tools-cache.outputs.cache-hit != 'true'
        run: scripts/install-ci-tools.sh "$RUNNER_TEMP/ci-tools"

      - name: Add CI tools to PATH
        run: echo "$RUNNER_TEMP/ci-tools" >> "$GITHUB_PATH"

      - name: Cache Rust build artifacts
        uses: Swatinem/rust-cache@6323deb102c322ba6fcbdcafc7e3dddab59af2b6
```

After static Helm validation, add:

```yaml
      - name: Set up Docker Buildx
        uses: docker/setup-buildx-action@8d2750c68a42422c14e847fe6c8ac0403b4cbd6f

      - name: Build the cached validation image
        uses: docker/build-push-action@10e90e3645eae34f1e60eeb005ba3a3d33f178e8
        with:
          context: .
          platforms: linux/amd64
          load: true
          push: false
          provenance: false
          tags: github-webhook-exporter:ci
          cache-from: type=gha,scope=production-image-linux-amd64
          cache-to: type=gha,mode=max,scope=production-image-linux-amd64

      - name: Smoke test the production image
        run: just image-smoke-loaded

      - name: Verify production image reproducibility
        if: github.event_name == 'push'
        run: just image-reproducibility-test
```

Use `just helm-kind-lifecycle-loaded` later in validation, delete `cargo build --locked`, and retain Clippy, tests, documentation, and uploads unchanged. In `publish-release`, change both BuildKit cache scope values to `production-image-linux-amd64` without changing metadata, authentication, or publication steps.

- [ ] **Step 4: Run workflow, shell, and YAML validation**

Run:

```bash
just workflow-test
shellcheck scripts/install-ci-tools.sh scripts/github-actions-test.sh
yq eval '.' .github/workflows/helm-package-ci.yml >/dev/null
```

Expected: all commands exit zero.

- [ ] **Step 5: Commit workflow caching**

```bash
git add .github/workflows/helm-package-ci.yml scripts/github-actions-test.sh
git commit -m "ci: cache and deduplicate Rust image builds"
```

---

### Task 4: Document policy and run complete verification

**Files:**
- Modify: `docs/operations.md:60-75`
- Create: `changelog/2026-08-11T00-43-23Z-ci-build-caching-implementation.md`

**Interfaces:**
- Consumes: the final workflow behavior from Task 3.
- Produces: operator-facing explanation of cached PR validation and uncached trusted-push reproducibility guarantees.

- [ ] **Step 1: Update the operations documentation**

Replace the opening GHCR release paragraph with:

```markdown
Pull requests and `main` are validation-only: they build and smoke-test the production image,
validate the packaged chart, and never authenticate to GHCR or publish a package. Validation uses
cargo-chef plus GitHub-hosted Cargo and BuildKit caches, and the smoke and Kind lifecycle checks
reuse one loaded image. A cache miss always performs a complete verified build. Pull requests omit
the expensive reproducibility comparison; pushes to `main` and stable release tags still perform
two cache-disabled builds and require identical image IDs. Temporary chart artifacts are retained
for 30 days through workflow artifacts.
```

- [ ] **Step 2: Add the timestamped changelog entry**

Create `changelog/2026-08-11T00-43-23Z-ci-build-caching-implementation.md` with:

```markdown
# Cache CI Rust and image builds

- Added cargo-chef dependency layers exported through GitHub's BuildKit cache.
- Cached checksum-pinned auxiliary CI tools and retained host Cargo artifact caching.
- Reused one validation image for smoke and Kind lifecycle checks.
- Limited two-build, cache-disabled reproducibility checks to `main` and release-tag pushes.
- Removed the redundant standalone host Cargo build from GitHub Actions.
```

- [ ] **Step 3: Run formatting, compilation, linting, tests, and documentation checks**

Run:

```bash
cargo fmt --all -- --check
cargo build --locked
cargo clippy --all-targets -- -D warnings
cargo test --all-targets
cargo doc --no-deps --locked
just workflow-test
```

Expected: every command exits zero with no compiler or Clippy warnings.

- [ ] **Step 4: Run full container verification**

Run:

```bash
just image-smoke
just image-reproducibility-test
```

Expected: smoke checks pass and both cache-disabled builds produce the same image ID.

- [ ] **Step 5: Check repository hygiene**

Run:

```bash
git diff --check
git status --short
```

Expected: no whitespace errors; only `docs/operations.md` and the new changelog file are uncommitted at this task boundary.

- [ ] **Step 6: Commit documentation**

```bash
git add docs/operations.md \
    changelog/2026-08-11T00-43-23Z-ci-build-caching-implementation.md
git commit -m "docs: explain CI build cache policy"
```

- [ ] **Step 7: Confirm the final branch state**

Run:

```bash
git status --short --branch
git log -5 --oneline
```

Expected: the worktree is clean and the four implementation commits follow the approved design commit.
