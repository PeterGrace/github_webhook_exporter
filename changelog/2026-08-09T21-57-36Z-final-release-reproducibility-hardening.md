# Final release reproducibility hardening

## What changed

- Declared `SOURCE_DATE_EPOCH` in the production builder and normalized every final `/out` artifact
  mtime after installation without changing ownership or modes.
- Added a cache-disabled, two-build image identity regression with deterministic commit-derived
  metadata, exact Docker image ID comparison, and test-tag cleanup.
- Enforced the new reproducibility gate in validation before release publication and disabled
  non-deterministic provenance on the locally loaded release image.
- Extended fake-registry publication coverage for malformed manifests, missing or invalid
  configuration digests, and the accepted `no such manifest` absence diagnostic.
- Moved registry command output into one private temporary directory with cleanup for normal exit,
  HUP, INT, or TERM termination.
- Corrected release operations guidance so existing images are never overwritten and can permit only
  chart-only recovery when the chart is absent and the image digest exactly matches.

## Verification

- `just release-publish-test`
- `just workflow-test`
- `just image-reproducibility-test`
- `just image-smoke`
```bash
shellcheck scripts/release-publish.sh scripts/release-publish-test.sh \
    scripts/image-reproducibility-test.sh scripts/github-actions-test.sh
```
- `git diff --check`
