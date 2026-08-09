# Version-Tag Image and Helm Chart Publication Design

## Purpose

Publish the production container image and Helm chart to GHCR only for stable repository tags, while
keeping pull-request and `main` workflows validation-only. Treat release versions as immutable and
support a narrowly guarded recovery when an image push succeeds but the chart push fails.

## Release contract

A release starts only from a repository tag in canonical `vMAJOR.MINOR.PATCH` form. Major, minor,
and patch components contain decimal digits without leading zeroes except for the value zero. The
normalized version is the tag without `v`.

Before registry authentication, the workflow must prove that the normalized version exactly equals:

- `package.version` in `Cargo.toml`;
- `version` in `charts/github-webhook-exporter/Chart.yaml`; and
- `appVersion` in `charts/github-webhook-exporter/Chart.yaml`.

Pull requests and pushes to `main` continue to run validation and may upload the existing temporary
Helm package as a GitHub Actions artifact with 30-day retention. They must have no GHCR login or
publication path. Non-version tags, prerelease tags, branch tags, SHA tags, and `latest` are not
published.

## Workflow architecture

The existing validation job remains the sole prerequisite for release publication. It runs the
repository-owned workflow contract, Helm static suite, production image smoke test, Kind lifecycle
suite, Rust formatting, build, Clippy, tests, and documentation generation. It uploads the exact
validated chart archive as a temporary Actions artifact.

A single dependent `publish-release` job handles both registry artifacts. Keeping publication in one
ordered job makes the preflight state decision explicit and prevents independent image and chart jobs
from racing. The job receives only `contents: read` and `packages: write`; workflow-level permissions
remain `contents: read`. Every third-party action is pinned to a full commit digest.

The publication job performs these operations in order:

1. Check out the release revision.
2. Validate and normalize the release version.
3. Install the repository-pinned Helm tooling.
4. Download the chart artifact created by the validation job.
5. Verify that the artifact contains exactly one archive whose version-derived filename, chart
   metadata, and rendered output match the release.
6. Build the `linux/amd64` image once under its normalized release tag.
7. Run the existing container smoke contract against that exact local image.
8. Authenticate to GHCR with the workflow-scoped `GITHUB_TOKEN`.
9. Inspect the remote image and chart release state.
10. Execute the permitted publication transition.

Version validation, chart validation, image construction, and image smoke testing all precede
registry authentication and every push. A failure in either artifact's validation therefore prevents
both publication commands from starting.

## Artifact coordinates

The image coordinate is:

```text
ghcr.io/petergrace/github-webhook-exporter:MAJOR.MINOR.PATCH
```

The chart is pushed with Helm to:

```text
oci://ghcr.io/petergrace/charts
```

and resolves as:

```text
oci://ghcr.io/petergrace/charts/github-webhook-exporter --version MAJOR.MINOR.PATCH
```

The chart archive consumed by publication is the exact archive uploaded by the validation job. The
archive filename is derived from validated chart metadata rather than fixed to the repository's
current `0.1.0` version. The workflow does not create a parallel, independently packaged chart.

## Immutability and recovery state machine

After authentication, the job inspects both versioned targets and chooses exactly one transition:

| Image state | Chart state | Outcome |
| --- | --- | --- |
| Missing | Missing | Push the image, then push the chart. |
| Present and digest matches the rebuilt image | Missing | Publish only the chart as guarded recovery. |
| Present and digest differs from the rebuilt image | Missing | Fail without publishing. |
| Missing | Present | Fail as inconsistent registry state. |
| Present | Present | Fail as an already-completed release. |

No transition overwrites or silently replaces an existing artifact. Helm-only recovery is permitted
only when the remote single-platform manifest's image configuration digest equals the local Docker
image ID of the rebuilt and smoke-tested release image. Docker image IDs are configuration digests,
so this comparison verifies the exact filesystem and runtime configuration without depending on a
locally unavailable registry manifest digest. The rerun remains tied to the original workflow
revision, so equality proves that the existing image is the artifact expected from that release
source. A multi-platform remote manifest, manually published image, or other digest mismatch causes
a conflict diagnostic and blocks the chart push.

Cross-artifact publication cannot be atomic in GHCR. A failure after the image push can therefore
leave a recoverable image-only state. The state machine makes that partial state explicit without
requiring a mutable tag or a new patch version. A chart-only state is never an expected workflow
result and fails closed.

## Components

### Release version validator

`scripts/release-version.sh` remains the authoritative metadata validator. Its tests cover canonical
stable tags, malformed and prerelease tags, leading zeroes, missing metadata, and each mismatch.

### Release registry helper

A focused repository-owned script will implement release-state inspection and publication decisions.
Its interface accepts the normalized version, local image reference, and validated chart archive.
It requires a single-platform remote image manifest and compares its configuration digest with the
local Docker image ID. External Docker and Helm commands remain injectable through `PATH`, allowing
tests to exercise real script control flow with deterministic command fixtures and no registry
access.

The helper emits bounded, actionable diagnostics that identify these cases without exposing tokens:
completed release, recoverable image-only release, image digest conflict, unexpected chart-only
release, inspection failure, image push failure, and chart push failure.

### Workflow contract test

`scripts/github-actions-test.sh` will validate the parsed workflow rather than relying only on text
searches. It will enforce triggers, job ordering, permissions, action digest pins, artifact transfer,
pre-authentication validation, publication command ordering, and absence of publication paths from
PR and `main` activity.

### Documentation

`docs/operations.md` and the chart README will document release-tag creation, version alignment,
image and chart coordinates, pull and install commands, immutable behavior, and guarded chart-only
recovery. Documentation will state that reruns are safe only when the existing image digest matches
the rebuilt release image and the chart version remains absent.

## Testing strategy

Development follows test-first cycles:

1. Extend workflow contract tests and observe failure before changing the workflow.
2. Add registry-helper tests for every state transition and command failure, observe failure, then
   implement the helper.
3. Add documentation contract assertions before updating operator documentation.
4. Run focused release, workflow, Helm package, and helper tests.
5. Run `just fmt`, `cargo clippy --all-targets -- -D warnings`, and `just test` from a clean sequence.
6. Run the issue-specific artifact gates: `cargo build --locked`, `cargo doc --no-deps --locked`,
   `just helm-static`, and `just image-smoke`.

Local tests must not authenticate to or publish into GHCR. Workflow execution through `act` is used
only if the selected event and secrets cannot reach publication; otherwise validation is limited to
the repository-owned contract tests and local artifact exercises.

## Security and failure handling

Registry credentials come only from `secrets.GITHUB_TOKEN` and are passed through established login
commands, never command arguments, committed files, logs, or generated artifacts. Publication
permissions exist only on the tag-gated publication job.

Inspection errors are distinct from confirmed absence. A timeout, authorization error, or malformed
registry response fails closed rather than being treated as a missing artifact. Every push command is
reachable only after all local validation and both remote-state inspections succeed.

## Out of scope

- Publication from pull requests, `main`, branches, or non-version tags.
- Mutable `latest`, branch, SHA, or prerelease tags.
- Multi-architecture images beyond `linux/amd64`.
- Signing, provenance attestations, SBOM publication, or external promotion.
- Atomic cross-artifact transactions, which GHCR does not provide.
- Changes to runtime, Helm deployment, persistence, exposure, or NetworkPolicy behavior.
