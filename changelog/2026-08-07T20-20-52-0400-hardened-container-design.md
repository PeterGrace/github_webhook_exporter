# Hardened production container design

- Selected a digest-pinned Rust Bookworm builder and distroless Debian runtime for the production
  image.
- Fixed this iteration's supported platform to `linux/amd64` and runtime identity to UID/GID 65532.
- Defined automated checks for startup, writable SQLite storage, direct SIGTERM handling, image
  metadata, credential hygiene, and exclusion of development artifacts.
