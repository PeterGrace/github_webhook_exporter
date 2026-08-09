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

# Validate rendered Helm manifests against Kubernetes schemas.
helm-kubeconform:
    scripts/helm-kubeconform.sh "{{helm-chart}}"

# Validate rendered Helm manifests against the workload security policy.
helm-policy:
    scripts/helm-policy-test.sh "{{helm-chart}}"

# Scan chart source, fixtures, and rendered output for credential leaks.
helm-secrets:
    scripts/helm-secret-scan.sh --test "{{helm-chart}}"

# Package the Helm chart and revalidate the extracted archive.
helm-package output-directory="dist":
    scripts/helm-package-test.sh "{{helm-chart}}" "{{output-directory}}"

# Run the full static Helm chart validation suite.
helm-static: helm-lint helm-test helm-kubeconform helm-policy helm-secrets helm-package

# Verify the GitHub Actions workflow contract.
workflow-test:
    scripts/github-actions-test.sh .github/workflows/helm-package-ci.yml

# Verify the rendered chart is accepted by a disposable Kind cluster.
helm-kind-acceptance:
    scripts/helm-kind-acceptance.sh "{{helm-chart}}"

# Build the supported linux/amd64 production image.
image-build:
    docker build --platform linux/amd64 --tag "{{container-image}}" .

# Build and exercise the production image contracts.
image-smoke: image-build
    scripts/container-smoke.sh "{{container-image}}"
