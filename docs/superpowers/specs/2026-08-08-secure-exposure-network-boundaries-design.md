# Secure Exposure and Network Boundaries Design

## Goal

Extend the singleton Helm chart with opt-in, independently controlled webhook, metrics, and
administrative exposure plus default-deny network boundaries. Preserve the single application
listener while making the limits of Kubernetes path enforcement explicit.

## Considered approaches

1. **Dedicated Services and fixed-path Ingresses (selected).** Render separate Services for metrics
   and administration and fixed-path Ingresses for webhook and administration. This gives platform
   routing and discovery resources clear purposes without claiming that Services isolate HTTP paths.
2. **One shared Service with multiple Ingresses.** This is smaller, but makes monitoring and
   management discovery less explicit and does not meet the dedicated-Service requirements.
3. **Bundled authorization proxy.** This could enforce paths strongly, but adds a runtime component,
   credentials, and lifecycle responsibilities explicitly outside this issue.

## Exposure resources

All new exposure is disabled by default.

- `webhookIngress` renders one `networking.k8s.io/v1` Ingress. Operators configure ingress class,
  host, annotations, and TLS metadata, but the backend path is fixed to `/webhooks/github` with
  `Exact` matching and targets the existing core Service.
- `metrics.service` renders a dedicated ClusterIP Service selecting the exporter pod and targeting
  its named `http` port. `metrics.serviceMonitor` optionally renders a
  `monitoring.coreos.com/v1` ServiceMonitor selecting that Service and scraping fixed path
  `/metrics`. Enabling the ServiceMonitor requires the metrics Service.
- `administration.service` renders a dedicated ClusterIP Service. `administration.ingress` renders
  a fixed `/api/v1/repositories` `Prefix` route to that Service and requires the administrative
  Service. Its class and metadata are separate from webhook ingress so operators can bind it to a
  management-only ingress controller.

Ingress TLS termination, certificate provisioning, controller installation, Prometheus Operator,
and authorization proxies remain platform responsibilities. The templates consume no Secret
values and never project existing-Secret keys into resource metadata.

## NetworkPolicy

`networkPolicy.enabled` renders one pod-selecting `networking.k8s.io/v1` NetworkPolicy with both
`Ingress` and `Egress` policy types. With no allowances enabled it is a default deny in both
directions.

Ingress allowances are separate, opt-in rules for ingress-controller, Prometheus, and management
traffic. Each rule takes explicit namespace and pod selectors and permits only TCP traffic to the
application port. Enabled rules require non-empty selectors so a missing override cannot silently
become an all-namespace or all-pod allowance.

Egress allowances are separate rules for cluster DNS and OTLP collectors. DNS defaults off and,
when enabled, permits TCP and UDP port 53 to configured namespace and pod selectors. OTLP accepts a
list of explicitly configured peers (namespace/pod selectors or CIDR blocks) and TCP ports. The
policy is independent of health probes because probes are inbound kubelet traffic and application
readiness never depends on collector reachability.

NetworkPolicy is pod/port based. Because every Service reaches the same pod listener, neither the
policy nor separate Services can distinguish `/webhooks/github`, `/metrics`, and
`/api/v1/repositories`. Operators needing path isolation must use fixed Ingress routing, a platform
authorization proxy, or equivalent external L7 policy.

## Validation

The shell render harness will follow red-green TDD:

- First assert that defaults render none of the new kinds.
- Add one failing fixture per independently enabled exposure resource and verify fixed paths,
  selectors, ports, and cross-resource requirements.
- Add representative selector overrides and assert exact default-deny plus bounded ingress, DNS,
  and OTLP egress rules.
- Scan rendered fixtures, chart examples, annotations, selectors, and endpoints for fixture
  credentials and OTLP header values.
- Run `helm lint`, `helm template`, and the existing chart harness, then the standard Rust project
  gates and documentation build.

## Scope boundaries

This change does not install controllers or CRDs, split the Rust listener, add an authorization
proxy, provide TLS certificates, or run disposable-cluster behavior tests.
