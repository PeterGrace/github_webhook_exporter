# Container image

The supported production image uses a digest-pinned Rust builder and a distroless Debian runtime.
This iteration supports `linux/amd64` only. It runs the application directly as PID 1 under the
fixed non-root identity `65532:65532`, exposes only TCP port 8080, and contains no shell or Rust
build tooling.

## Building

```bash
just image-build
```

builds the default `github-webhook-exporter:dev` tag locally. Set `CONTAINER_IMAGE` for a release
or registry-specific tag — the same value is used by the smoke verification:

```bash
CONTAINER_IMAGE=registry.example/github-webhook-exporter:X.Y.Z just image-build
CONTAINER_IMAGE=registry.example/github-webhook-exporter:X.Y.Z just image-smoke
```

## Filesystem contract

The working and data directory is `/var/lib/github-webhook-exporter`. Mount persistent storage at
that path and set `GHE_DATABASE_PATH` to a file within it, such as
`/var/lib/github-webhook-exporter/github-webhook-exporter.db`. An empty Docker volume inherits the
directory's ownership from the image; other volume providers must make the mount writable by UID
and GID `65532`.

## Required environment

| Variable | Contract |
| --- | --- |
| `GHE_DATABASE_PATH` | Writable SQLite file path, normally below the mounted data directory. |
| `GHE_MASTER_KEY` | Base64 encoding of exactly 32 random bytes, from a secret store. |
| `GHE_ADMIN_TOKEN` | Non-empty administrator bearer token, from a secret store. |

Optional `GHE_*`, `OTEL_*`, and `RUST_LOG` variables retain the contracts in
[Environment variables](environment-variables.md). Never place secret values in image arguments,
labels, Dockerfiles, or committed manifests.

## Health checks

The image deliberately defines no Docker `HEALTHCHECK`. Orchestrators should use
`GET /health/live` and `GET /health/ready` — see [HTTP API](http-api.md). Because the binary is the
direct entrypoint, `SIGTERM` reaches the application's graceful lifecycle without a shell
intermediary. Set the orchestrator termination grace period greater than the sum of
`GHE_SHUTDOWN_TIMEOUT_SECONDS` and `GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS` so both application work and
telemetry providers receive their full shutdown boundaries — see
[Startup, retention, and shutdown](lifecycle.md).
