# Release and packaging

## GHCR publication

Pull requests and `main` are validation-only: they build and smoke-test the production image,
validate the packaged chart, and never authenticate to GHCR or publish a package. Validation uses
cargo-chef plus GitHub-hosted Cargo and BuildKit caches, and the smoke and Kind lifecycle checks
reuse one loaded image. A cache miss always performs a complete verified build. Pull requests omit
the expensive reproducibility comparison; pushes to `main` and stable release tags still perform
two cache-disabled builds and require identical image IDs. Temporary chart artifacts are retained
for 30 days through workflow artifacts.

A stable `vMAJOR.MINOR.PATCH` repository tag publishes one immutable `linux/amd64` image and one
Helm OCI chart only after full validation passes. The release workflow requires the tag without
`v`, the Cargo package version, the Helm chart version, and the Helm `appVersion` to match exactly
before it authenticates. See [How to release a new version](../how-to/release-a-new-version.md)
for the procedure that keeps all four version fields aligned.

For example, after all four version fields are `0.1.4`, consume the published image and chart:

```bash
docker pull ghcr.io/petergrace/github-webhook-exporter:0.1.4
helm pull oci://ghcr.io/petergrace/charts/github-webhook-exporter --version 0.1.4
helm install github-webhook-exporter oci://ghcr.io/petergrace/charts/github-webhook-exporter --version 0.1.4
```

Published version tags are immutable. Existing image tags are never overwritten. An exact matching
existing image permits chart-only recovery only when the chart is absent. The workflow never
publishes `latest`, branch, SHA, or prerelease tags. The overwrite guard is not atomic with the
registry push; repository administrators must prevent concurrent or manual pushes to release tags,
because GHCR does not enforce this repository policy on its own. If validation fails, rerun the
original failed workflow attempt without moving the tag. Only the image-existing/chart-missing
state with an exact matching digest may resume as chart-only recovery. Completed, chart-only, and
digest-conflict states fail closed without overwrite.

### Image and chart state matrix

| Image state | Chart state | Result |
| --- | --- | --- |
| missing | missing | Publish the immutable image first, then the chart after validation. |
| matching digest | missing | Chart-only recovery; rerun the original failed workflow attempt without moving the tag after confirming the remote image configuration digest exactly matches the rebuilt image. |
| different digest | missing | Fail closed; digest-conflict state fails closed without overwrite. |
| missing | present | Fail closed; chart-only registry state fails closed without overwrite. |
| matching digest | present | Completed; fail closed without overwrite. |
| different digest | present | Fail closed; digest-conflict state fails closed without overwrite. |

The only resumable state is image-existing/chart-missing with an exact digest match. Any completed
publication, chart-only registry state, or digest conflict must not be overwritten.

## Helm package validation and maintenance

Prerequisites: `just`, `helm`, `docker`, `kubeconform`, `conftest`, and `yq` on `PATH`. The pinned
CI install script keeps those versions aligned with the workflow contract.

```bash
just helm-static
just image-smoke
just workflow-test
just helm-maintenance-unit
just helm-kind-acceptance
KIND_ARTIFACT_DIRECTORY=dist/kind-lifecycle just helm-kind-lifecycle
```

`just helm-static` validates chart metadata, rendering, schema, policy, secret, and packaged
archive contracts across the supported Kubernetes range 1.31.0 through 1.35.0
(`>=1.31.0-0 <1.36.0-0`). `just image-smoke` builds and exercises the production image locally.
`just workflow-test` checks the GitHub Actions contract, including the exact archive path
`dist/github-webhook-exporter-0.1.4.tgz`. `just helm-kind-acceptance` confirms API acceptance for
the rendered StatefulSet, Service, ConfigMap, and PVC; it does not start the exporter.

`just helm-kind-lifecycle` builds and loads the `linux/amd64` production image into a uniquely
named disposable Kind cluster. It creates private test credentials at runtime, installs the chart,
and proves probes, repository administration, signed webhooks, bounded metrics, SQLite
persistence, delivery deduplication, pull-request queue completion across restart,
collector-failure isolation, broken-storage readiness, bounded SIGTERM, singleton PVC rollout
behavior, online backup, stopped restore, restored metadata, and post-recovery encrypted
repository, deduplication, queue, and metric behavior. Its default diagnostics directory is
`dist/kind-lifecycle`; set `KIND_ARTIFACT_DIRECTORY` to isolate concurrent runs. The suite replaces
that directory at startup and scans rendered objects, status records, events, descriptions, and
logs for generated credentials, signatures, and forbidden payload material before success.

The cluster and private temporary files are removed after both success and failure. For
interactive debugging only, `KEEP_KIND_CLUSTER=true` preserves them and prints the generated
cluster name and kubeconfig path; delete that cluster and temporary directory manually as soon as
investigation finishes. CI uses checksum-pinned Kind 0.31.0 and kubectl 1.35.0, while the harness
pins the Kind Kubernetes 1.35.0 node image by digest. CI uploads the diagnostics for 14 days with
`if: always()`.

For local archive inspection, use the fixed package name directly:

```bash
helm show chart dist/github-webhook-exporter-0.1.4.tgz
helm show values dist/github-webhook-exporter-0.1.4.tgz
helm template archive dist/github-webhook-exporter-0.1.4.tgz --kube-version 1.35.0 >/dev/null
```

If `just helm-policy` fails, run `just helm-render` first, then inspect the rendered manifests
under `dist/rendered/` and compare them with the bounded policy fixtures under
`ci/helm/negative/policy/`. If `just helm-secrets` fails, inspect the rendered manifests and ensure
the chart never copies credentials into ConfigMaps, Services, Ingresses, ServiceMonitors, or
NetworkPolicies. The chart README documents the exact Secret contract for runtime credentials.

When updating CI tools, edit `ci/tool-versions.env` and the corresponding checksums together, then
re-run `scripts/install-ci-tools.sh` or `just workflow-test` to confirm the pinned downloads still
validate. Do not rely on runner-provided tools or silently accept checksum drift.

Static validation only covers packaging, render, schema, policy, and smoke-test contracts.
passing static checks does not prove cluster lifecycle behavior. Runtime readiness and rollout
remain a cluster responsibility — see [How to validate the Helm package](../how-to/validate-the-helm-package.md).
