# CI build caching final review fixes

- Removed Docker BuildKit target cache mounts so cargo-chef cook, application build, and release assembly persist `/build/target` in image layers.
- Tightened the workflow contract to reject `/build/target` cache mounts and require assembly from `target/release/github_webhook_exporter`.
- Reordered validation setup so cache misses rely on `scripts/install-ci-tools.sh` for Rust 1.97.1, while warm tool-cache hits still install the exact pinned toolchain in-workflow.
- Preserved immutable action pins, release publication auth boundaries, runtime image contracts, and push-only reproducibility gating.
