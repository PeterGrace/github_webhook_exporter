# Helm Packaging and Policy CI Design

## Goal

Add deterministic continuous-integration validation for the production image and every supported
Helm rendering introduced by issues #43 through #45. Static checks must reject chart/schema errors,
unsupported Kubernetes resources, unsafe workload settings, embedded credentials, and packaging
regressions before any cluster-level acceptance test runs.

## Considered approaches

1. **Focused Helm, kubeconform, and Conftest pipeline (selected).** Keep rendering, schema checks,
   Rego policy, secret scanning, and packaging behind repository-owned scripts and `just` recipes.
   This provides actionable local failures and uses the same entry points in CI and later cluster
   jobs without introducing a chart-management framework.
2. **Bash and yq assertions only.** This minimizes dependencies but duplicates Kubernetes schema
   semantics and makes security policy harder to review and extend safely.
3. **The Helm chart-testing framework.** This is conventional for repositories containing many
   charts, but its repository/change-discovery machinery is unnecessary for this single chart and
   would obscure the exact validation contracts required here.

## Toolchain and reproducibility

CI installs immutable versions rather than using runner-provided or mutable `latest` tools:

- Helm `v4.2.3` for linting, rendering, and packaging.
- kubeconform `v0.8.0` for Kubernetes-version-aware manifest validation.
- Conftest `v0.69.0` for Rego policy checks.
- yq `v4.53.3` for deterministic YAML selection and assertions.
- ShellCheck `v0.11.0` for repository shell validation.
- just `1.58.0` for the repository task interface.
- Rust `1.97.1`, matching the production image builder, for project gates.

GitHub Actions dependencies are pinned to full commit SHAs. Downloaded tool archives are verified
against release checksums before installation. The workflow records each tool version before using
it so failures show the exact validator set.

The chart declares Kubernetes `>=1.31.0-0 <1.36.0-0` support. kubeconform validates all built-in
objects against Kubernetes `1.31.0` and `1.35.0`, the lower and upper supported minor versions. A repository-owned, versioned ServiceMonitor
schema validates the optional Prometheus Operator object without installing its CRD or depending on
mutable remote schema catalogs.

## Render matrix

Repository-owned values fixtures define a bounded matrix rather than an exponential cross-product.
The matrix includes:

- defaults, including OTLP disabled and no optional resources;
- persistence storage class and capacity overrides;
- external Secret key references, including OTLP header references without Secret values;
- OTLP endpoints enabled;
- the optional singleton-safe PodDisruptionBudget;
- webhook Ingress;
- metrics Service and ServiceMonitor;
- administration Service and Ingress;
- default-deny NetworkPolicy; and
- selector-bounded ingress plus DNS and OTLP egress policy.

One rendering script produces stable, named manifest files from these fixtures. Every downstream
schema, policy, and secret check consumes those exact files. This avoids independent tools silently
validating different chart combinations. Existing detailed chart contract tests remain in place and
continue to test boundary values and template diagnostics.

## Policy validation

Conftest policies inspect rendered workloads and reject:

- replicas other than exactly one;
- root execution or missing `runAsNonRoot` settings;
- privileged containers or privilege escalation;
- host PID, IPC, or network namespaces;
- hostPath volumes;
- writable root filesystems;
- added Linux capabilities or failure to drop `ALL`;
- missing CPU or memory requests and limits;
- an automounted service account token; and
- unsafe service-account overrides.

Each rule has a minimal negative YAML fixture. The policy test harness first proves all supported
matrix renders pass, then proves every negative fixture fails and emits its expected rule identifier.
This ensures a missing or renamed policy cannot turn a negative test into a false success.

## Credential scanning

A focused scanner checks values fixtures, source chart files, every rendered manifest, and the
unpacked packaged chart. It rejects known fixture tokens, master-key and webhook-secret values,
Authorization or OTLP header values, Kubernetes Secret objects, and credential-shaped example
assignments. Secret names and `secretKeyRef` key names remain allowed because they are references,
not credentials.

Negative fixtures cover each prohibited credential category and are expected to fail with an
actionable category-specific diagnostic. Production scans exclude the negative fixture directory so
intentional test credentials cannot contaminate the chart-package check.

## Packaging and image validation

Focused `just` recipes expose linting, matrix rendering, kubeconform validation, policy checks,
secret scans, chart packaging, and an aggregate static gate. Packaging writes a predictable archive
under `dist/`, then exercises it with `helm show chart`, `helm show values`, and the same render
matrix used for the source chart. Generated output is ignored by Git.

The existing image build and smoke recipes remain the production-image contract. CI invokes those
recipes after static chart validation, proving the linux/amd64 distroless image starts, reports
ready, persists SQLite state, runs as UID/GID 65532, and shuts down cleanly. The chart archive is
uploaded only after static, image, and standard project gates pass.

## Workflow and failure behavior

A pull-request and main-branch GitHub Actions workflow runs in this order:

1. check out the exact commit;
2. install and report pinned tools;
3. lint shell scripts;
4. run the aggregate Helm static gate;
5. build and smoke-test the production image through existing `just` recipes;
6. run `just fmt`, `cargo build`, Clippy with warnings denied, `just test`, and
   `cargo doc --no-deps`; and
7. upload the verified chart archive as a CI artifact.

Scripts use strict shell mode, temporary directories with cleanup traps, deterministic fixture
names, and category-specific diagnostics. No check writes credentials to logs. Cluster installation,
public publication, signing, attestations, SBOM publication, and release promotion remain outside
this issue.

## Validation

Development follows red-green testing for each validation boundary:

- add matrix expectations before the render implementation;
- add each negative policy fixture before its Rego rule;
- add each negative credential fixture before its scanner category;
- make packaged-chart tests fail before adding archive validation; and
- make the workflow contract test fail before adding the workflow.

Final local validation runs the exact focused recipes, production image smoke test, standard Rust
project gates, and archive render checks. If Docker is unavailable, image validation is reported as
blocked rather than silently skipped.
