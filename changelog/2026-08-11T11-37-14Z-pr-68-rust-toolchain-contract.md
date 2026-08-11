# Align PR 68 Rust toolchain contracts

- Derived the workflow's expected Rust toolchain from `ci/tool-versions.env`.
- Added a contract check that rejects cache-hit/cache-miss Rust version skew.
- Added a contract check that keeps rustup profile and component options aligned between the workflow and CI tool installer.
- Verified both guards with mutation-based failing checks before restoring the valid configuration.
