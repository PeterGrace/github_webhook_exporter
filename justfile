container-image := env_var_or_default("CONTAINER_IMAGE", "github-webhook-exporter:dev")
helm-chart := "charts/github-webhook-exporter"

# Verify Rust formatting without modifying source files.
fmt:
    cargo fmt --all -- --check

# Run every library, binary, and integration test target.
test:
    cargo test --all-targets

# Validate the Helm chart metadata, defaults, and templates.
helm-lint:
    helm lint "{{helm-chart}}"

# Render the deterministic supported Helm matrix.
helm-render output-directory="dist/rendered":
    scripts/helm-render-matrix.sh "{{helm-chart}}" "{{output-directory}}"

# Exercise Helm chart schema, rendering, and Secret argument contracts.
helm-test:
    scripts/helm-chart-test.sh "{{helm-chart}}"
    scripts/helm-kind-secret-argv-test.sh scripts/helm-kind-acceptance.sh "{{helm-chart}}"

# Validate the exact shared render matrix against Kubernetes schemas.
helm-kubeconform: helm-render
    scripts/helm-kubeconform.sh "dist/rendered"

# Validate the exact shared render matrix against the workload security policy.
helm-policy: helm-render
    scripts/helm-policy-test.sh "dist/rendered"

# Scan chart source, fixtures, and the exact shared render matrix for credentials.
helm-secrets: helm-render
    scripts/helm-secret-scan.sh --test "{{helm-chart}}" "dist/rendered"

# Package the Helm chart and revalidate the extracted archive.
helm-package output-directory="dist":
    scripts/helm-package-test.sh "{{helm-chart}}" "{{output-directory}}"

# Run the full static Helm chart validation suite.
helm-static: helm-lint helm-test helm-kubeconform helm-policy helm-secrets helm-package

# Run focused Helm output, archive, installer, and workflow security regressions.
helm-security-test:
    scripts/helm-security-self-test.sh

# Verify release tag and package version alignment.
release-version-test:
    scripts/release-version-test.sh

# Verify immutable image and Helm chart publication transitions.
release-publish-test:
    scripts/release-publish-test.sh

# Verify the GitHub Actions workflow contract.
workflow-test:
    scripts/github-actions-test.sh .github/workflows/helm-package-ci.yml

# Verify deterministic Kind lifecycle helper contracts.
helm-kind-lifecycle-unit:
    scripts/helm-kind-lifecycle-lib-test.sh

# Exercise lifecycle and persistence in a disposable Kind cluster.
helm-kind-lifecycle: image-build
    scripts/helm-kind-lifecycle.sh "{{helm-chart}}" "{{container-image}}" \
        "${KIND_ARTIFACT_DIRECTORY:-dist/kind-lifecycle}"

# Verify the rendered chart is accepted by a disposable Kind cluster.
helm-kind-acceptance:
    scripts/helm-kind-acceptance.sh "{{helm-chart}}"

# Build the supported linux/amd64 production image.
image-build:
    docker build --platform linux/amd64 --tag "{{container-image}}" .

# Build and exercise the production image contracts.
image-smoke: image-build
    scripts/container-smoke.sh "{{container-image}}"
