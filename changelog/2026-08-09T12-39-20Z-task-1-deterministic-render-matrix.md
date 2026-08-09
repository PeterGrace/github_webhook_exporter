# Task 1: Deterministic supported render matrix

## Summary
- Added the canonical Helm render matrix contract at `ci/helm/render-cases.txt`.
- Added deterministic render fixtures for persistence, external secret wiring, OTLP endpoints,
  PDB, webhook ingress, metrics, administration, and bounded network policy coverage.
- Added `scripts/helm-render-matrix.sh` to render each case with `--kube-version 1.31.0`, recreate
  the output directory, and verify exactly one StatefulSet and no Secret objects per render.
- Added the chart Kubernetes version guard in `charts/github-webhook-exporter/Chart.yaml` and a
  `just helm-render` recipe.

## Evidence
- RED: `just helm-render /tmp/gwe-render-matrix`
- GREEN: `just helm-render /tmp/gwe-render-matrix`
- GREEN: `test "$(find /tmp/gwe-render-matrix -name '*.yaml' | wc -l)" -eq 10`
- GREEN: `scripts/helm-chart-test.sh charts/github-webhook-exporter`
- RED: `helm template test charts/github-webhook-exporter --kube-version 1.30.0`
