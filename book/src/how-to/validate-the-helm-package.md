# How to validate the Helm package before shipping

Run these from the repository root before you tag a release or hand a chart change to review.
Install `just`, `helm`, `docker`, `kubeconform`, `conftest`, and `yq` first — the pinned CI install
script (`scripts/install-ci-tools.sh`) keeps their versions aligned with what CI actually runs.

```bash
just helm-static
just image-smoke
just workflow-test
just helm-maintenance-unit
just helm-kind-acceptance
KIND_ARTIFACT_DIRECTORY=dist/kind-lifecycle just helm-kind-lifecycle
```

- `just helm-static` validates chart metadata, rendering, schema, policy, secret, and packaged
  archive contracts.
- `just image-smoke` builds and exercises the production image locally.
- `just workflow-test` checks the GitHub Actions workflow contract itself.
- `just helm-kind-acceptance` confirms the rendered StatefulSet, Service, ConfigMap, and PVC are
  API-acceptable; it does not start the exporter.
- `just helm-kind-lifecycle` builds the production image, loads it into a disposable Kind cluster,
  installs the chart, and drives the exporter through probes, repository administration, signed
  webhooks, metrics, persistence, deduplication, queue completion across restart, collector-failure
  isolation, bounded shutdown, and a full backup/restore cycle. Its diagnostics land in
  `dist/kind-lifecycle` by default — set `KIND_ARTIFACT_DIRECTORY` to isolate concurrent runs.

Static validation only proves packaging, render, schema, policy, and smoke-test contracts —
passing it does not prove cluster lifecycle behavior. Only `helm-kind-lifecycle` does that.

## If `just helm-policy` fails

Run `just helm-render` first, then compare the rendered manifests under `dist/rendered/` against
the bounded policy fixtures under `ci/helm/negative/policy/`.

## If `just helm-secrets` fails

Inspect the rendered manifests and confirm the chart never copies credentials into ConfigMaps,
Services, Ingresses, ServiceMonitors, or NetworkPolicies — only the Secret you supply should carry
them.

## Inspecting a built archive locally

```bash
helm show chart dist/github-webhook-exporter-*.tgz
helm show values dist/github-webhook-exporter-*.tgz
helm template archive dist/github-webhook-exporter-*.tgz --kube-version 1.35.0 >/dev/null
```

## Full contract details

[Release and packaging](../reference/release-and-packaging.md) documents the exact supported
Kubernetes version range, the pinned CI tool versions, and the image/chart publication state
matrix these checks enforce.
