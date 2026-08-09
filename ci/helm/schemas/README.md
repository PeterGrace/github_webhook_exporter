# Helm schema mirrors

This directory vendors the Kubernetes and custom resource schemas used by
`scripts/helm-kubeconform.sh`.

## ServiceMonitor schema

- Repository: https://github.com/datreeio/CRDs-catalog
- Commit: `52b0261318acc7dd0b66e032759b1f218216b980`
- Source path: `monitoring.coreos.com/servicemonitor_v1.json`
- License: MIT
- File: `monitoring.coreos.com/servicemonitor_v1.json`

## Kubernetes built-in schemas

The built-in Kubernetes object schemas are mirrored into the versioned
`v1.31.0-standalone-strict/` and `v1.35.0-standalone-strict/` directories so
kubeconform can validate the supported matrix without reaching mutable remote
schema catalogs.

## Update procedure

1. Refresh the Kubernetes built-in schemas for the supported boundary versions.
2. Refresh `monitoring.coreos.com/servicemonitor_v1.json` from the pinned
   CRDs-catalog commit above.
3. Re-run `just helm-kubeconform`.
4. Commit the schema changes together with any validator updates.
