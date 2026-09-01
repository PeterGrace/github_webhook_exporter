# Helm chart support for branch-protection required checks

**Issue:** [#95](https://github.com/PeterGrace/github_webhook_exporter/issues/95)
**Follows:** [2026-09-01T00-00-00Z-workflow-required-check-attribute.md](2026-09-01T00-00-00Z-workflow-required-check-attribute.md)

The application change landed first with the chart gap called out explicitly. This closes it, so a
Helm-deployed exporter can enable required-check lookups without an out-of-band environment patch.

## What shipped

- `githubApp.appId`, `githubApp.installationId`, `githubApp.apiBaseUrl`.
- `application.requiredCheckTtlSeconds` (default `300`, range `1..=86400`).
- `existingSecret.keys.githubAppPrivateKey`, projecting the PEM from the existing Secret.
- `networkPolicy.egress.github`, mirroring the OTLP rule's shape.
- A `github-app` render-matrix case, plus a GitHub egress rule in the `network-policy-bounded`
  fixture so the new rule passes kubeconform, the workload policy, and the secret scan.

## Decisions worth recording

**The private key is projected as a file, never as an environment variable.** Kubernetes writes a
Secret value into a mounted file verbatim, so a mounted file *is* the PEM an operator already holds.
The application's `GHE_GITHUB_APP_PRIVATE_KEY` form expects base64, which would mean the Secret had
to contain base64-of-PEM — double-encoded relative to every other consumer of that Secret, and a
predictable support burden. The chart therefore supports only `GHE_GITHUB_APP_PRIVATE_KEY_PATH`,
mounts the key read-only under `/etc/github-webhook-exporter/github-app`, and sets the path itself.
The base64 form remains available for non-Kubernetes deployments. As a side benefit the key never
enters the pod's environment at all, which is the stronger posture regardless of encoding.

**`defaultMode` is written as decimal `288`, not `0440`.** This was a live bug caught while
inspecting the rendered JSON. YAML 1.2 parsers — including the `yq` this repository pins — read a
leading zero as part of a *decimal* number, so `0440` renders as `440`, which Kubernetes interprets
as octal `0670`: group-writable. The decimal literal is unambiguous under both YAML 1.1 and 1.2, and
the template carries a comment saying so, because `288` is otherwise a magic number begging to be
"fixed" back into `0440`.

**Partial GitHub App configuration fails at render time.** The three settings are all-or-nothing.
The application already rejects a partial configuration at startup; failing in `helm template`
surfaces the same mistake before a rollout rather than during one.

**The GitHub egress rule reuses the OTLP peer shape rather than inventing one.** Both rules are now
`{enabled, peers, ports}` over the same `egressPeer` definition — renamed from `otlpPeer`, since two
rules share it — rendered by one `egressPeers` template helper and validated by one loop over both
rules. GitHub's API is outside the cluster, so its peers are normally an `ipBlock` for an egress
gateway; NetworkPolicy matches addresses, not hostnames, and the README says so rather than leaving
operators to discover that `api.github.com` is not something a policy can name.

## Validation

`just helm-static` passes in full: lint, chart contract tests, kubeconform against Kubernetes 1.31
and 1.35, the workload security policy, the structural secret scan, and packaging.

The secret scan initially rejected the chart's own new validation message: its format string
contained `existingSecret.keys.githubAppPrivateKey=%v`, and any assignment whose left-hand side ends
in `privatekey` is treated as an embedded credential. The scanner was right to be blunt about it, so
the diagnostic now reports whether the key entry is configured instead of echoing the name beside a
value. That reasoning is recorded in a comment at the site, since the message otherwise looks
gratuitously inconsistent with its neighbors.
