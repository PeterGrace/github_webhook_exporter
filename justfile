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

# Verify the rendered chart is accepted by a disposable Kind cluster.
helm-kind-acceptance:
    scripts/helm-kind-acceptance.sh "{{helm-chart}}"

# Build the supported linux/amd64 production image.
image-build:
    docker build --platform linux/amd64 --tag "{{container-image}}" .

# Build and exercise the production image contracts.
image-smoke: image-build
    scripts/container-smoke.sh "{{container-image}}"
