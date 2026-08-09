# Helm packaging and policy CI design

- Selected a focused Helm, kubeconform, and Conftest validation pipeline for issue #46.
- Defined pinned tool versions, a bounded chart render matrix, Kubernetes schema validation, policy
  and credential negative fixtures, packaged-chart checks, and production-image smoke validation.
- Kept cluster lifecycle testing, publication, signing, attestations, and release promotion outside
  this iteration.
