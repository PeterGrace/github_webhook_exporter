# NetworkPolicy Probe Documentation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Clarify the probe behavior of the chart's default-deny NetworkPolicy using standard Kubernetes semantics while warning about separate CNI host-firewall controls.

**Architecture:** Keep the rendered NetworkPolicy unchanged because Kubernetes NetworkPolicy always permits traffic from the selected pod's node. Lock the operator-facing statement into the Helm test harness, then align the chart README and existing design rationale with that distinction.

**Tech Stack:** Helm 3, Bash, Markdown, Kubernetes `networking.k8s.io/v1` NetworkPolicy.

## Global Constraints

- Do not add a kubelet peer rule to the rendered NetworkPolicy.
- Distinguish standard Kubernetes NetworkPolicy from CNI-specific host-firewall extensions.
- State that operators using additional host-firewall controls must independently permit kubelet probes.
- Keep the change documentation-only apart from its render-harness assertion.
- Add a timestamped changelog entry.

---

### Task 1: Correct and lock probe guidance

**Files:**
- Modify: `scripts/helm-chart-test.sh:932-939`
- Modify: `charts/github-webhook-exporter/README.md:180-189`
- Modify: `docs/superpowers/specs/2026-08-08-secure-exposure-network-boundaries-design.md:50-54`
- Create: `changelog/2026-08-09T00-34-03Z-network-policy-probe-guidance.md`

**Interfaces:**
- Consumes: Kubernetes NetworkPolicy's guarantee that ingress from the pod's node remains allowed.
- Produces: Tested operator guidance distinguishing standard policy behavior from CNI host-firewall policy.

- [ ] **Step 1: Add a failing documentation assertion**

Add this assertion beside the existing README NetworkPolicy assertions:

```bash
assert_contains \
    'CNI-specific host-firewall' \
    "${CHART_DIRECTORY}/README.md" \
    'README must distinguish CNI host-firewall controls from standard NetworkPolicy'
```

- [ ] **Step 2: Run the focused Helm test and verify failure**

Run: `just helm-test`

Expected: FAIL with `README must distinguish CNI host-firewall controls from standard NetworkPolicy`.

- [ ] **Step 3: Add the minimal documentation correction**

In `charts/github-webhook-exporter/README.md`, state that standard Kubernetes NetworkPolicy permits ingress from the pod's node, so kubelet liveness and readiness probes need no chart allowance. Immediately state that CNI-specific host-firewall policies are separate controls and must independently permit kubelet probes.

In the existing design spec, replace the ambiguous sentence about probe independence with the same standard-versus-extension distinction.

Create the timestamped changelog entry summarizing the clarification and unchanged manifest behavior.

- [ ] **Step 4: Run project validation**

Run:

```bash
just helm-lint
just helm-test
just fmt
cargo build
cargo clippy --all-targets -- -D warnings
just test
cargo doc --no-deps
git diff --check
```

Expected: every command exits successfully with no warnings or test failures.

- [ ] **Step 5: Commit, push, and reply to the review thread**

```bash
git add scripts/helm-chart-test.sh \
    charts/github-webhook-exporter/README.md \
    docs/superpowers/specs/2026-08-08-secure-exposure-network-boundaries-design.md \
    changelog/2026-08-09T00-34-03Z-network-policy-probe-guidance.md \
    docs/superpowers/plans/2026-08-08-network-policy-probe-documentation.md
git commit -m "docs: clarify NetworkPolicy probe behavior"
git push origin feat-issue-45-secure-exposure-boundaries
```

Reply in review thread `3741689334`, explaining that upstream Kubernetes guarantees traffic from the pod's node under standard NetworkPolicy and that the docs now separately warn operators about CNI host-firewall controls.
