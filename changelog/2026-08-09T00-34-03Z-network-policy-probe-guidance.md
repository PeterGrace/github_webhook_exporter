# Clarify NetworkPolicy probe behavior

- Document that standard Kubernetes NetworkPolicy permits traffic from a pod's node, so the chart does not need an explicit kubelet probe allowance.
- Warn operators that CNI-specific host-firewall controls are separate and must independently permit liveness and readiness probes.
- Keep the rendered NetworkPolicy unchanged and add a Helm documentation assertion for the distinction.
