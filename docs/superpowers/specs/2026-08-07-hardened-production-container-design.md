# Hardened Production Container Design

## Goal

Provide a reproducible, production-supported `linux/amd64` container image for
`github_webhook_exporter` that preserves the service's existing environment, HTTP, SQLite, and
signal contracts while running without root privileges or development tooling.

## Scope

This change adds the image build, local image validation recipes, operator documentation, and a
changelog entry. Helm resources, registry publishing, signing, SBOM publication, release
automation, and architectures other than `linux/amd64` remain out of scope.

## Image architecture

The Dockerfile uses two stages:

1. An amd64 Rust 1.97.1 Bookworm builder pinned to manifest digest
   `sha256:e544a8ee0b93bb2ddc8c67a80606f040998eff3847e4deed988d0874559f52a8`.
   It copies the Cargo manifests, `.cargo/config.toml`, source, and embedded migrations, then uses
   BuildKit cache mounts while building the locked release binary with
   `cargo build --locked --release`.
2. An amd64 `gcr.io/distroless/cc-debian12:nonroot` runtime pinned to manifest digest
   `sha256:471dbca9cad607b9a32c10e9c31fb09ffaeb2d460e0afbff86c27abbc80b1b98`.
   It receives only the compiled binary and a pre-created application data directory.

The final image runs as the distroless fixed identity `65532:65532`. Its working and writable data
directory is `/var/lib/github-webhook-exporter`, owned by that identity. The binary is the direct
entrypoint so it remains PID 1 and receives SIGTERM without a shell intermediary. The image exposes
only TCP port 8080 and defines no image-level health check.

A narrow allowlist-based `.dockerignore` sends only `Cargo.toml`, `Cargo.lock`,
`.cargo/config.toml`, `src/`, and `migrations/` as build inputs.

## Runtime contract

The container does not embed application configuration or secrets. Operators must provide:

- `GHE_DATABASE_PATH`, normally `/var/lib/github-webhook-exporter/github-webhook-exporter.db`;
- `GHE_MASTER_KEY`;
- `GHE_ADMIN_TOKEN`.

All existing optional `GHE_*`, `OTEL_*`, and `RUST_LOG` variables retain their application-defined
behavior. The service listens on port 8080 by default. Deployments must mount persistent storage at
`/var/lib/github-webhook-exporter` when SQLite state must survive replacement.

Image tags are caller-selected through a `just` variable and default to
`github-webhook-exporter:dev`. The supported build and runtime platform for this iteration is
`linux/amd64` only.

## Build and verification

The `justfile` gains one recipe to build the image and one smoke recipe to verify the delivered
artifact. Verification uses Docker metadata and container execution to prove:

- the configured image user is `65532:65532`;
- the image architecture is amd64 and port 8080 is exposed;
- the application can initialize SQLite in the mounted data directory and serve readiness;
- files created in the data directory are owned by UID/GID 65532;
- SIGTERM reaches the direct application process and shutdown completes within the configured HTTP,
  retention, and telemetry timeout boundary;
- image configuration and history do not contain fixture credential values;
- the final filesystem contains no Rust compiler, Cargo executable or registry, source tree, or
  shell.

Smoke-test credentials are generated at runtime, supplied only to `docker run`, and never written to
the Dockerfile, image layers, image configuration, or repository. Cleanup traps remove temporary
containers and host data after success or failure.

## Error handling

Docker build and smoke commands run with fail-fast shell behavior. Readiness polling has a bounded
timeout and prints container logs on failure. Shutdown verification also has a bounded deadline;
exceeding it fails the recipe instead of forcefully reporting success. Cleanup remains best-effort so
the original verification failure is preserved.

## Documentation

`docs/operations.md` documents the image tag parameter, amd64-only support, port, non-root identity,
data directory, required environment variables, persistence mount, build command, smoke command,
and Kubernetes shutdown-grace relationship. A timestamped file under `changelog/` records the
iteration.
