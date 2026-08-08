# Hardened Production Container Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver a reproducible, non-root, distroless `linux/amd64` image with automated build and runtime verification.

**Architecture:** A digest-pinned Rust Bookworm stage builds the locked release binary using BuildKit caches. A digest-pinned distroless Debian stage contains only that binary and an owned SQLite data directory; a host-side smoke script verifies image metadata, filesystem contents, startup, persistence, and bounded SIGTERM handling.

**Tech Stack:** Docker BuildKit, Rust 1.97.1, Debian 12 distroless, Bash, just, curl.

## Global Constraints

- Support `linux/amd64` only in this iteration.
- Pin the builder to `rust:1.97.1-bookworm@sha256:e544a8ee0b93bb2ddc8c67a80606f040998eff3847e4deed988d0874559f52a8`.
- Pin the runtime to `gcr.io/distroless/cc-debian12:nonroot@sha256:471dbca9cad607b9a32c10e9c31fb09ffaeb2d460e0afbff86c27abbc80b1b98`.
- Run as UID/GID `65532:65532` with `/var/lib/github-webhook-exporter` writable by that identity.
- Keep the application binary as PID 1 and expose only TCP port 8080.
- Do not embed secrets, configuration values, a shell, development tools, source, or Cargo state.
- Do not add an image-level health check.
- Preserve all existing environment, startup, health, SQLite, and signal contracts.

---

### Task 1: Executable container acceptance test

**Files:**
- Create: `scripts/container-smoke.sh`
- Modify: `justfile`
- Test: `scripts/container-smoke.sh`

**Interfaces:**
- Consumes: a local image reference as positional argument `$1`, Docker, curl, tar, timeout, base64.
- Produces: `just image-smoke`, which builds the default image and exits nonzero on any contract violation.

- [ ] **Step 1: Write the smoke test before the image implementation**

Create a strict Bash script that:

1. validates its image argument and required host commands;
2. creates collision-resistant container and volume names and installs a cleanup trap;
3. asserts Docker metadata reports amd64, user `65532:65532`, direct entrypoint
   `/usr/local/bin/github_webhook_exporter`, no command wrapper, and only `8080/tcp` exposed;
4. exports a created container filesystem and rejects shell paths, `rustc`, `cargo`, Cargo registry,
   `Cargo.toml`, Rust source files, and a build source tree;
5. generates a 32-byte base64 master key and independent administrator token and proves neither
   appears in image inspect output or image history;
6. starts the image with a named volume, a random localhost port, required environment variables,
   two-second HTTP/retention shutdown and one-second telemetry shutdown boundaries;
7. polls `/health/ready` with a bounded deadline and prints logs if startup fails;
8. sends SIGTERM, bounds `docker wait` to six seconds, and requires exit status zero;
9. starts a second container against the same volume and reaches readiness again, proving the
   non-root process can create and reopen SQLite state on the mounted data directory;
10. terminates the second container cleanly and removes all test resources.

Add `container-image := env_var_or_default("CONTAINER_IMAGE", "github-webhook-exporter:dev")`, an
`image-build` recipe, and an `image-smoke` recipe depending on `image-build`.

- [ ] **Step 2: Validate the test script and verify RED**

Run:

```bash
bash -n scripts/container-smoke.sh
just image-smoke
```

Expected: syntax validation passes; `just image-smoke` fails because `Dockerfile` does not exist.
This is the expected RED state proving the acceptance path exercises the missing artifact.

- [ ] **Step 3: Commit the failing acceptance test**

```bash
git add scripts/container-smoke.sh justfile
git commit -m "test: define production image contracts"
```

### Task 2: Digest-pinned distroless image

**Files:**
- Create: `Dockerfile`
- Create: `.dockerignore`
- Test: `scripts/container-smoke.sh`

**Interfaces:**
- Consumes: `Cargo.toml`, `Cargo.lock`, `.cargo/config.toml`, `src/`, and `migrations/`.
- Produces: `/usr/local/bin/github_webhook_exporter`, direct OCI entrypoint, and owned
  `/var/lib/github-webhook-exporter` in `github-webhook-exporter:dev` by default.

- [ ] **Step 1: Add the minimal multi-stage Dockerfile**

Use Dockerfile syntax 1.7. Force each stage to `linux/amd64`. In the pinned Rust stage, copy only the
allowlisted build inputs, mount locked Cargo registry and target caches, run
`cargo build --locked --release`, and copy the resulting binary into an `/out` root filesystem with
mode `0555`. Create `/out/var/lib/github-webhook-exporter` with mode `0700` and owner/group 65532.

In the pinned distroless stage, copy `/out/` while preserving numeric ownership and modes. Set the
working directory to `/var/lib/github-webhook-exporter`, set `USER 65532:65532`, expose `8080/tcp`,
and set JSON-form `ENTRYPOINT` directly to `/usr/local/bin/github_webhook_exporter`. Do not define
`CMD`, `ENV`, `ARG` for credentials, `VOLUME`, or `HEALTHCHECK`.

- [ ] **Step 2: Restrict the build context**

Create an allowlist-based `.dockerignore` beginning with `**`, then re-include only:

```text
Cargo.toml
Cargo.lock
.cargo/config.toml
src/**
migrations/**
```

Re-include each parent directory so Docker can traverse it.

- [ ] **Step 3: Verify GREEN on the delivered artifact**

Run:

```bash
just image-smoke
```

Expected: the image builds and every metadata, filesystem, startup, persistence, secret-hygiene,
and SIGTERM assertion passes.

- [ ] **Step 4: Inspect final image size and linked runtime contract**

Run:

```bash
docker image inspect github-webhook-exporter:dev \
  --format 'size={{.Size}} user={{.Config.User}} arch={{.Architecture}}'
docker history --no-trunc github-webhook-exporter:dev
```

Expected: amd64 and `65532:65532`; history contains only the distroless base, copied runtime root,
and OCI configuration, with no credentials or builder commands in final layers.

- [ ] **Step 5: Commit the image implementation**

```bash
git add Dockerfile .dockerignore
git commit -m "feat: add hardened production image"
```

### Task 3: Operator contract and release record

**Files:**
- Modify: `docs/operations.md`
- Create: `changelog/<timestamp>-hardened-production-container.md`

**Interfaces:**
- Consumes: the image and recipe contracts from Tasks 1 and 2.
- Produces: operator instructions for build, smoke verification, runtime configuration, persistence,
  and shutdown grace periods.

- [ ] **Step 1: Document container operations**

Add a production-container section to `docs/operations.md` covering:

- `CONTAINER_IMAGE=... just image-build` and `CONTAINER_IMAGE=... just image-smoke`;
- the default `github-webhook-exporter:dev` tag and caller-selected release tags;
- amd64-only support;
- UID/GID 65532, port 8080, and `/var/lib/github-webhook-exporter`;
- required `GHE_DATABASE_PATH`, `GHE_MASTER_KEY`, and `GHE_ADMIN_TOKEN` variables without example
  secret values;
- persistent storage mounted at the data directory;
- direct PID 1 SIGTERM behavior and a Kubernetes termination grace period greater than the sum of
  `GHE_SHUTDOWN_TIMEOUT_SECONDS` and `GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS`;
- the absence of a Docker health check and continued use of `/health/live` and `/health/ready` by
  orchestrator probes.

- [ ] **Step 2: Add the required timestamped changelog**

Record the pinned multi-stage image, non-root runtime/data directory, just recipes, amd64 scope,
automated runtime verification, and operator documentation. Do not duplicate the earlier design
record.

- [ ] **Step 3: Run the complete validation sequence**

Run from the repository root, in order:

```bash
just fmt
cargo build
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps
just image-smoke
git diff --check origin/main...HEAD
```

Expected: every command exits zero with no compiler or Clippy warnings, and the fresh image passes
all runtime checks.

- [ ] **Step 4: Commit documentation**

```bash
git add docs/operations.md changelog/
git commit -m "docs: document production container operations"
```

### Task 4: PR delivery

**Files:**
- No additional repository files.

**Interfaces:**
- Consumes: validated commits from Tasks 1 through 3.
- Produces: pushed branch, pull request against `main`, and issue #43 timeline link.

- [ ] **Step 1: Verify repository state and commit history**

Run:

```bash
git status --short
git log --oneline origin/main..HEAD
```

Expected: clean working tree and focused design, test, implementation, and documentation commits.

- [ ] **Step 2: Push and open the pull request**

Push `feat-issue-43-hardened-container-image`, open a PR against `main` titled
`feat: add a hardened production container image`, include actual validation evidence, and include
`Closes #43`.

- [ ] **Step 3: Comment on issue #43**

Post the PR number and URL to the issue timeline.
