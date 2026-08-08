# Secure Helm exposure and network boundaries

- Added disabled-by-default webhook and administrative Ingress resources with fixed application paths.
- Added dedicated metrics and administrative Services plus optional Prometheus ServiceMonitor integration.
- Added default-deny NetworkPolicy rendering with explicit ingress-controller, Prometheus, management, DNS, and OTLP allowances.
- Documented the shared-listener path-enforcement limitation and platform-owned TLS and L7 policy responsibilities.
- Expanded Helm render, schema, selector, path, and sensitive-content regression coverage.
