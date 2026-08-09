# PR #57 Buildx review response

- Changed the production-image reproducibility test to use `docker buildx build --load`, matching
  the release workflow's Buildx export path instead of relying on the `docker build` frontend.
- Added a workflow contract assertion that prevents the regression test from drifting away from the
  release build path.
- Documented the single-platform configuration-digest equality that guards chart-only recovery.
