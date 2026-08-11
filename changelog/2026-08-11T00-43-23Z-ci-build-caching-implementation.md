# Cache CI Rust and image builds

- Added cargo-chef dependency layers exported through GitHub's BuildKit cache.
- Cached checksum-pinned auxiliary CI tools and retained host Cargo artifact caching.
- Reused one validation image for smoke and Kind lifecycle checks.
- Limited two-build, cache-disabled reproducibility checks to `main` and release-tag pushes.
- Removed the redundant standalone host Cargo build from GitHub Actions.
