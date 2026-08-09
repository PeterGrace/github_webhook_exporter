# Release publication chart marker hardening

## What changed
- Hardened `scripts/release-publish.sh` chart inspection so only the exact Helm OCI not-found marker for `ghcr.io/petergrace/charts/github-webhook-exporter:${VERSION}` is treated as confirmed absence.
- Updated `scripts/release-publish-test.sh` so the fake missing-chart response emits the exact chart/version marker and added a generic `configuration file not found` fixture that must fail closed.

## Why it changed
- Prevent unrelated lowercase `not found` diagnostics from being misclassified as chart absence and incorrectly allowing publication.
