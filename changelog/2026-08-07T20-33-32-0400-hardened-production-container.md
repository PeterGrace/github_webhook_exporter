# Hardened production container image

- Added a reproducible `linux/amd64` multi-stage build with digest-pinned Rust 1.97.1 Bookworm and
  distroless Debian inputs.
- Restricted the final image to the release binary, runtime libraries, and a UID/GID 65532-owned
  SQLite data directory.
- Added `just image-build` and `just image-smoke` recipes with automated metadata, filesystem,
  readiness, persistent-volume, credential-hygiene, and SIGTERM checks.
- Documented image tags, architecture, port, non-root identity, runtime directory, required
  environment variables, persistent storage, probes, and shutdown grace periods.
