# Helm kubeconform validation

- Added strict kubeconform validation for the rendered Helm matrix at Kubernetes 1.31.0 and 1.35.0.
- Vendored the ServiceMonitor schema from the pinned CRDs-catalog commit.
- Added an unsupported `extensions/v1beta1` Ingress negative fixture.
- Wired `just helm-kubeconform` to the new validation script.
