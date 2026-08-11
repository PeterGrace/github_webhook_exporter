# CI cache and trusted-push gating

## Summary
- added pinned Rust toolchain installation plus runner-scoped `actions/cache` restoration for `RUNNER_TEMP/ci-tools`
- switched validation to a single cached linux/amd64 Buildx image build followed by `just image-smoke-loaded` and `just helm-kind-lifecycle-loaded`
- gated `just image-reproducibility-test` to trusted `push` events only and aligned release BuildKit cache scopes to `production-image-linux-amd64`
- updated workflow contract tests and the workflow-order security fixture for the new validation step ordering

## Verification
- `just workflow-test`
- `shellcheck scripts/install-ci-tools.sh scripts/github-actions-test.sh scripts/helm-security-self-test.sh`
- `yq eval '.' .github/workflows/helm-package-ci.yml >/dev/null`
- `scripts/helm-security-self-test.sh`
