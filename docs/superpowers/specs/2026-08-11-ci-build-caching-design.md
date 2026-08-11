# CI Build Caching Design

## Objective

Reduce GitHub Actions validation and release time by avoiding redundant Rust and container
compilation while preserving the existing security, release, and reproducibility guarantees.

## Current State

The `validate` and `publish-release` jobs are defined in
`.github/workflows/helm-package-ci.yml`.

Validation currently:

- restores host-side Cargo artifacts with `Swatinem/rust-cache`;
- builds the production container for the smoke test;
- performs two cache-disabled container builds for the reproducibility test;
- requests another image build before the Kind lifecycle test;
- runs a standalone host `cargo build` before Clippy, tests, and documentation.

The release job already uses GitHub's BuildKit cache backend. The Dockerfile uses BuildKit cache
mounts for Cargo's registry and target directory, but those mutable mounts are not a dependable
cross-run dependency cache. A source change also invalidates the Dockerfile's single application
build layer.

## Selected Approach

Use cargo-chef to isolate container dependency compilation, export BuildKit layers through the
GitHub Actions cache backend, build the PR validation image once, and reserve the intentionally
cache-disabled reproducibility check for trusted pushes to `main` and release tags.

This retains the canonical Docker build path. Building a host binary and injecting it into the
container is out of scope because that would weaken validation of the production Dockerfile.

## Workflow Design

### Shared setup

The validation job will:

1. Check out the repository.
2. Install the pinned Rust toolchain independently of the auxiliary CI tools.
3. Restore the auxiliary tool directory with `actions/cache`, using a key derived from the runner
   platform, `ci/tool-versions.env`, and `scripts/install-ci-tools.sh`.
4. Run the checksum-verifying tool installer only on a tool-cache miss.
5. Restore host Cargo artifacts with the existing pinned `Swatinem/rust-cache` action.
6. Configure Docker Buildx before any container build.

All third-party actions remain pinned to immutable commit SHAs. Cache keys contain no secrets.
GitHub's branch cache isolation remains in effect for pull requests.

### Validation image

A pinned `docker/build-push-action` invocation will build and load
`github-webhook-exporter:ci` once. It will restore and export a `mode=max` GitHub Actions BuildKit
cache under one stable linux/amd64 production-image scope.

The smoke and Kind lifecycle checks will consume the loaded image without rebuilding it. Dedicated
Just recipes will make the distinction explicit:

- a recipe that smoke-tests an already-loaded image;
- a recipe that runs the Kind lifecycle test against an already-loaded image.

Existing developer-facing recipes that build before testing will remain available for local use.

### Reproducibility

`just image-reproducibility-test` will run only when `github.event_name == 'push'`. Given the
workflow triggers, this means pushes to `main` and semantic-version tags.

The test will continue to build twice with `--no-cache` and compare image IDs. The condition will
not be relaxed for release tags. This preserves detection of nondeterministic image assembly while
removing the most expensive intentional rebuilds from ordinary pull requests.

### Host Rust checks

The standalone `cargo build --locked` step will be removed. The production image build, Clippy,
all-target tests, and documentation generation already exercise compilation for their respective
purposes. Clippy, tests, and documentation remain separate because they produce different compiler
artifacts and enforce distinct contracts. `Swatinem/rust-cache` will continue to share reusable
host artifacts between runs.

### Release publication

The release image build will retain its immutable metadata and smoke test. Its existing GitHub
Actions BuildKit cache configuration will use the same linux/amd64 cache scope as validation, so a
tag validation can warm dependency and application layers before publication. Release-specific
labels and build arguments may invalidate final image layers without forcing dependency
recompilation.

## Dockerfile Design

The builder will use cargo-chef `0.1.71`, installed with `--locked` on top of the existing
SHA-pinned Rust base image. It will have three logical phases:

1. **Planner:** copy the build inputs and generate `recipe.json`.
2. **Dependency builder:** cook the release dependencies from `recipe.json`.
3. **Application builder:** copy the real source and migrations, then compile and install the
   release binary.

The cooked dependency output will be a regular BuildKit layer, allowing `cache-to: type=gha,
mode=max` to persist it across runners. Cargo registry and target cache mounts may remain as
complementary local acceleration, but correctness must not depend on them. The distroless runtime
stage and its ownership, permissions, entrypoint, and reproducible timestamps remain unchanged.

## Cache Invalidation

- Auxiliary CI tools invalidate when their versions, checksums, or installer change.
- Host Cargo artifacts invalidate through `Swatinem/rust-cache` when Rust, Cargo inputs, or cache
  configuration change.
- Container dependencies invalidate when cargo-chef's recipe, Cargo lockfile, Rust image, Cargo
  configuration, or cargo-chef version changes.
- Application layers invalidate when source code or migrations change.
- Final release layers invalidate when release metadata or `SOURCE_DATE_EPOCH` changes.

No fallback key may cross operating systems or architectures.

## Failure Handling

A cache miss is normal and triggers a full verified build. Cache restoration or export must never
skip validation commands. Invalid auxiliary tool downloads still fail checksum verification. A
failed image build, smoke test, Kind lifecycle test, Clippy run, test, documentation build, or
reproducibility comparison remains fatal.

## Contract Testing

Update `scripts/github-actions-test.sh` to assert:

- immutable pins and keys for the tool and Rust caches;
- Buildx setup before the validation image build;
- GitHub Actions cache import and `mode=max` export on validation and release image builds;
- exactly one cache-enabled validation image build;
- smoke and Kind checks use the already-loaded image;
- the reproducibility step has the push-only condition and retains its two no-cache builds;
- the redundant standalone host build is absent;
- release publication permissions and authentication boundaries are unchanged.

Docker and shell contract tests will cover the cargo-chef planner/cook structure and the new Just
recipes. Existing container smoke, reproducibility, release, and workflow tests remain authoritative.

## Verification

Implementation is complete when all of the following pass:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --all-targets`
- `just workflow-test`
- relevant Dockerfile and Just recipe contract tests
- `just image-smoke`
- `just image-reproducibility-test`

After merge, GitHub Actions logs should show a cold successful run followed by a cache-restored run.
A pull request must perform one production image build and no reproducibility build; a `main` or tag
push must still execute both cache-disabled reproducibility builds.

## Out of Scope

- Weakening or removing the reproducibility comparison.
- Injecting host-built binaries into production images.
- Splitting the validation job into a larger job matrix.
- Caching Kind cluster state or runtime test data.
- Changing release tags, image metadata, registry authentication, or publication permissions.
