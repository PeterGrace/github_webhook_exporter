# Tag-only image and Helm chart publication

## What changed

- Restricted release publication to stable semantic-version-shaped tags while preserving validation for pull requests and `main` pushes.
- Replaced the image-only publication job with a dependent release job that downloads exactly the validated `helm-package` artifact.
- Added pre-authentication gates for release-version agreement, the expected chart archive name and version, Kubernetes 1.35.0 chart rendering, and the exact normalized local image smoke test.
- Kept workflow-wide permissions read-only and granted `packages: write` only to the tag-gated publication job.
- Authenticated Docker and Helm to GHCR with `GITHUB_TOKEN`, passing the Helm token over standard input, before delegating immutable image and Helm OCI publication to `scripts/release-publish.sh`.
- Made release image identity deterministic across workflow reruns by deriving the OCI creation label and `SOURCE_DATE_EPOCH` from the checked-out release commit.

## Safety properties

Pull requests and `main` pushes remain validation-only. Every action is pinned to an exact commit, the transferred chart is validated before either registry login, and the build action loads without pushing. OCI metadata and BuildKit timestamps use stable RFC3339 and Unix timestamps from `GITHUB_SHA`, so digest-verified chart-only recovery can recognize a previously pushed image after a rerun. The publication helper remains the only release push state machine, including fail-closed remote inspection and digest-verified chart-only recovery.
