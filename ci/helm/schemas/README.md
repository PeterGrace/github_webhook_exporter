# Helm schema mirrors

This directory vendors the immutable Kubernetes and custom-resource schemas used by
`scripts/helm-kubeconform.sh`; validation never reaches a remote schema catalog.

## ServiceMonitor schema

- Repository: <https://github.com/datreeio/CRDs-catalog>
- Revision: `52b0261318acc7dd0b66e032759b1f218216b980`
- Source path: `monitoring.coreos.com/servicemonitor_v1.json`
- License: MIT (`LICENSE` at the revision above)
- Upstream SHA-256: `8978f86e2a7cb281a9ca7bc30d857e0553666658dfe062078c51e99d3f20cd14`
- Committed SHA-256: `0d33b38eae7a364500e0add3ef5f7fe8879bbd7c71e402c1e85a3fd758112a32`

The committed copy adds root `additionalProperties: false` because the immutable upstream schema
was strict below `spec` but not at the document root. The focused negative fixture
`ci/helm/negative/schema/servicemonitor-top-level-typo.yaml` protects that local hardening.

## Kubernetes built-in schemas

The six built-in schemas needed by the rendered matrix come from:

- Repository: <https://github.com/yannh/kubernetes-json-schema>
- Revision: `c8f4e61c63bc529749125ac566bccc6986e08d45`
- Source paths: `v1.31.0-standalone-strict/<file>` and
  `v1.35.0-standalone-strict/<file>`
- License: Apache License 2.0 (`LICENSE` at the revision above)

Committed SHA-256 checksums:

```text
e0eaddebd677c08aa092b2da2264d86ac4fc34eed112b9fac2945b3f00c1e9b1  v1.31.0-standalone-strict/configmap-v1.json
4e0f63ad84c2bf22565e489d1f4b885ddaa9f6bf7cff1ddd562553760afe4d79  v1.31.0-standalone-strict/ingress-networking-v1.json
68f66caa6cb28841e7ab6b2b1cf5ac56085d50730a3813e538dd9204529b5b04  v1.31.0-standalone-strict/networkpolicy-networking-v1.json
9f72ca6ac7baa59ce19de22e9817b0ec91ae3f061343acd212c70c511a40e10b  v1.31.0-standalone-strict/poddisruptionbudget-policy-v1.json
f489d6102675238b913898caf6fef6f472403950fc9e5895ef718f3c4f1c4351  v1.31.0-standalone-strict/service-v1.json
8f4a138cfead2499492eb8f08569ade2e004f52c0538555d305bacc52f1f7ba2  v1.31.0-standalone-strict/statefulset-apps-v1.json
e0eaddebd677c08aa092b2da2264d86ac4fc34eed112b9fac2945b3f00c1e9b1  v1.35.0-standalone-strict/configmap-v1.json
4e0f63ad84c2bf22565e489d1f4b885ddaa9f6bf7cff1ddd562553760afe4d79  v1.35.0-standalone-strict/ingress-networking-v1.json
f6324cc464f62228b0418f438d167208e4f86c7e3677ba30f608e79a8b26ba79  v1.35.0-standalone-strict/networkpolicy-networking-v1.json
da73f50ad0264d73f668eecaa9959da65afe2846602a7a5c8fd51f7799d6a258  v1.35.0-standalone-strict/poddisruptionbudget-policy-v1.json
8bf019854daed511e7c174896a898173fa65d88ec5937c687a37303d4cc9351b  v1.35.0-standalone-strict/service-v1.json
da78ed86dce07983c3e54c565def2c2e478fd458155ac7cd56cf671e88e16ee2  v1.35.0-standalone-strict/statefulset-apps-v1.json
```

## Exact refresh procedure

Choose and review new immutable revisions first, then run from the repository root:

```bash
set -euo pipefail
builtin_revision=c8f4e61c63bc529749125ac566bccc6986e08d45
servicemonitor_revision=52b0261318acc7dd0b66e032759b1f218216b980
files=(configmap-v1.json ingress-networking-v1.json networkpolicy-networking-v1.json \
  poddisruptionbudget-policy-v1.json service-v1.json statefulset-apps-v1.json)
for version in 1.31.0 1.35.0; do
  for file in "${files[@]}"; do
    curl -fsSLo "ci/helm/schemas/v${version}-standalone-strict/${file}" \
      "https://raw.githubusercontent.com/yannh/kubernetes-json-schema/${builtin_revision}/v${version}-standalone-strict/${file}"
  done
done
curl -fsSLo ci/helm/schemas/monitoring.coreos.com/servicemonitor_v1.json \
  "https://raw.githubusercontent.com/datreeio/CRDs-catalog/${servicemonitor_revision}/monitoring.coreos.com/servicemonitor_v1.json"
```

Reapply and review the documented ServiceMonitor root hardening, update the revisions and all
checksums in this file, then run
`find ci/helm/schemas -name '*.json' -print0 | sort -z | xargs -0 sha256sum`,
`just helm-kubeconform`, and `just helm-static`. Commit schema, provenance, fixture, and validator
changes together.
