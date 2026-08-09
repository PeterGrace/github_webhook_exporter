# Secure Exposure and Network Boundaries Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add opt-in Helm exposure resources and selector-bounded default-deny network policies for webhook, metrics, and administration traffic.

**Architecture:** Extend the strict chart values/schema with purpose-specific exposure groups. Render focused templates for webhook Ingress, metrics Service/ServiceMonitor, administration Service/Ingress, and one combined NetworkPolicy while retaining the single exporter pod listener.

**Tech Stack:** Helm 3, Kubernetes `networking.k8s.io/v1`, Prometheus Operator `monitoring.coreos.com/v1`, Bash, yq.

## Global Constraints

- Every new resource is disabled by default.
- Webhook routing is fixed to exact path `/webhooks/github`.
- Metrics scraping is fixed to `/metrics`; administration routing is fixed to prefix `/api/v1/repositories`.
- NetworkPolicy provides only pod/port enforcement and must not be documented as HTTP path isolation.
- Secret values and OTLP headers must never enter annotations, labels, Ingress, ServiceMonitor, NetworkPolicy, examples, or rendered fixtures.
- Do not install controllers, CRDs, certificates, collectors, DNS implementations, or authorization proxies.

---

### Task 1: Exposure values and schema

**Files:**
- Modify: `scripts/helm-chart-test.sh`
- Modify: `charts/github-webhook-exporter/values.yaml`
- Modify: `charts/github-webhook-exporter/values.schema.json`
- Modify: `charts/github-webhook-exporter/templates/_helpers.tpl`

**Interfaces:**
- Consumes: existing chart root values and strict `additionalProperties: false` schema.
- Produces: `webhookIngress`, `metrics`, and `administration` value groups consumed by templates.

- [ ] **Step 1: Write failing default and schema tests**

Add default manifest assertions for zero `Ingress` and zero `ServiceMonitor` objects. Add
`expect_failure` calls for unknown exposure keys, ServiceMonitor without metrics Service, and
administrative Ingress without administrative Service.

- [ ] **Step 2: Verify the tests fail for missing schema keys**

Run: `just helm-test`
Expected: FAIL because `webhookIngress`, `metrics`, and `administration` are rejected as additional
properties or required validation is absent.

- [ ] **Step 3: Add minimal typed defaults and schema**

Use disabled defaults with explicit ports, ingress class/host/annotations/TLS metadata, and
ServiceMonitor interval/scrape timeout/labels. Add strict object schemas, Kubernetes port ranges,
and hostname/string constraints. Add Helm validation requiring each Ingress/monitor backend Service.

- [ ] **Step 4: Verify schema tests pass**

Run: `just helm-test`
Expected: PASS for defaults and expected rejection diagnostics.

- [ ] **Step 5: Commit**

```bash
git add scripts/helm-chart-test.sh charts/github-webhook-exporter/values.yaml \
  charts/github-webhook-exporter/values.schema.json charts/github-webhook-exporter/templates/_helpers.tpl
git commit -m "feat: add secure exposure chart values"
```

### Task 2: Fixed-path exposure templates

**Files:**
- Create: `charts/github-webhook-exporter/templates/webhook-ingress.yaml`
- Create: `charts/github-webhook-exporter/templates/metrics-service.yaml`
- Create: `charts/github-webhook-exporter/templates/servicemonitor.yaml`
- Create: `charts/github-webhook-exporter/templates/administration-service.yaml`
- Create: `charts/github-webhook-exporter/templates/administration-ingress.yaml`
- Modify: `scripts/helm-chart-test.sh`

**Interfaces:**
- Consumes: Task 1 exposure values and existing `fullname`, `labels`, and `selectorLabels` helpers.
- Produces: independently opt-in exposure objects with fixed HTTP paths and pod selectors.

- [ ] **Step 1: Add failing render fixtures**

Render each enablement combination and assert exact object counts. Assert webhook Ingress has only
`Exact /webhooks/github`, administrative Ingress has only `Prefix /api/v1/repositories`, both use
the expected Service, metrics Service selects exporter pods, and ServiceMonitor selects only the
metrics Service with endpoint path `/metrics`.

- [ ] **Step 2: Verify templates are absent**

Run: `just helm-test`
Expected: FAIL because enabled fixtures contain no new resources.

- [ ] **Step 3: Implement minimal templates**

Render annotations only when non-empty, `ingressClassName` only when configured, hostless rules when
host is empty, and TLS only from explicit non-secret metadata. Reuse common labels and stable pod
selectors. Never read `.Values.existingSecret` in these templates.

- [ ] **Step 4: Verify exposure fixtures pass**

Run: `just helm-test`
Expected: PASS with fixed routes and exact selectors.

- [ ] **Step 5: Commit**

```bash
git add charts/github-webhook-exporter/templates scripts/helm-chart-test.sh
git commit -m "feat: render opt-in Helm exposure resources"
```

### Task 3: Default-deny and bounded NetworkPolicy

**Files:**
- Modify: `charts/github-webhook-exporter/values.yaml`
- Modify: `charts/github-webhook-exporter/values.schema.json`
- Modify: `charts/github-webhook-exporter/templates/_helpers.tpl`
- Create: `charts/github-webhook-exporter/templates/networkpolicy.yaml`
- Modify: `scripts/helm-chart-test.sh`

**Interfaces:**
- Consumes: application Service port plus typed namespace/pod/IP block selectors and TCP/UDP ports.
- Produces: one opt-in NetworkPolicy with `Ingress` and `Egress` isolation and only configured rules.

- [ ] **Step 1: Add failing policy render tests**

Assert defaults render no policy. Assert enabled policy has both policy types and empty rule arrays.
Render ingress-controller, Prometheus, and management selector overrides and compare exact peers and
TCP application port. Render DNS selectors with TCP/UDP 53 and multiple OTLP collector peers/ports.
Add schema failures for enabled allowances with empty selectors, missing OTLP peers, invalid CIDRs,
and invalid ports.

- [ ] **Step 2: Verify policy tests fail**

Run: `just helm-test`
Expected: FAIL because `networkPolicy` is not accepted and no policy renders.

- [ ] **Step 3: Add typed policy values, schema, validation, and template**

Keep all allowances disabled by default. Require explicit non-empty namespace/pod selectors for
inbound and DNS rules. Represent OTLP peers as either namespace/pod selector objects or `ipBlock`
objects, and render only configured TCP ports. Use the exporter selector labels for `podSelector`.

- [ ] **Step 4: Verify exact policy output passes**

Run: `just helm-test`
Expected: PASS for default deny, all bounded allowances, and schema rejection fixtures.

- [ ] **Step 5: Commit**

```bash
git add charts/github-webhook-exporter scripts/helm-chart-test.sh
git commit -m "feat: add bounded exporter network policies"
```

### Task 4: Documentation and sensitive-data scans

**Files:**
- Modify: `charts/github-webhook-exporter/README.md`
- Modify: `charts/github-webhook-exporter/templates/NOTES.txt`
- Modify: `scripts/helm-chart-test.sh`
- Create: `changelog/2026-08-08T<timestamp>Z-secure-exposure-network-boundaries.md`

**Interfaces:**
- Consumes: all new chart values and rendered resource contracts.
- Produces: operator examples and regression scans with no usable credentials.

- [ ] **Step 1: Add failing documentation and scan assertions**

Require README examples for webhook ingress, Prometheus, management, DNS, and OTLP flows. Require
explicit statements that Services and NetworkPolicy cannot enforce HTTP paths and name Ingress,
authorization proxy, or external L7 policy as enforcement choices. Expand sensitive scans to include
fixture credential markers and OTLP header values across rendered metadata, selectors, endpoints,
and examples.

- [ ] **Step 2: Verify documentation assertions fail**

Run: `just helm-test`
Expected: FAIL because the README still says the chart supplies no exposure resources.

- [ ] **Step 3: Write operator documentation and changelog**

Document disabled defaults, prerequisites, concrete values examples, fixed paths, TLS ownership,
selector requirements, DNS/OTLP flows, and the L3/L4 path limitation. Keep examples free of Secret
values and credential-like placeholders.

- [ ] **Step 4: Verify chart documentation tests pass**

Run: `just helm-test`
Expected: PASS with all scans clean.

- [ ] **Step 5: Commit**

```bash
git add charts/github-webhook-exporter/README.md charts/github-webhook-exporter/templates/NOTES.txt \
  scripts/helm-chart-test.sh changelog
git commit -m "docs: explain secure Helm exposure controls"
```

### Task 5: Full validation and PR delivery

**Files:**
- Modify only if validation identifies a scoped defect.

**Interfaces:**
- Consumes: completed chart and tests.
- Produces: validated branch ready for review.

- [ ] **Step 1: Run chart artifact checks**

```bash
command -v helm
command -v yq
just helm-lint
just helm-test
helm template github-webhook-exporter charts/github-webhook-exporter >/tmp/gwe-45-default.yaml
```

Expected: all commands exit zero; default output contains none of the opt-in exposure/policy kinds.

- [ ] **Step 2: Run standard project gates in order**

```bash
just fmt
cargo build
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps
```

Expected: every command exits zero without warnings.

- [ ] **Step 3: Review branch scope and sensitive content**

```bash
git diff origin/main...HEAD --check
git status --short
git log --oneline origin/main..HEAD
```

Expected: only issue #45 files and commits are present; working tree is clean.

- [ ] **Step 4: Push and open the issue-linked PR**

Use branch `feat-issue-45-secure-exposure-boundaries`, include actual validation results, and close
issue #45 from the PR body.
