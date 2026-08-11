# CI build caching design

- Documented the approved cargo-chef and GitHub Actions caching architecture.
- Limited cache-disabled image reproducibility builds to `main` and release-tag pushes.
- Specified a single cached validation image shared by smoke and Kind lifecycle checks.
- Preserved release, security, and deterministic image contracts.
