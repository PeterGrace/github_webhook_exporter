# Release operations and recovery documentation

## What changed
- Added workflow contract assertions in `scripts/github-actions-test.sh` for the Helm OCI chart
  coordinate, pull/install examples, digest-guarded chart-only recovery, and the release state
  table markers.
- Extended `docs/operations.md` with stable tag release steps, the Helm OCI chart coordinate,
  pull/install commands, the image/chart state matrix, 30-day validation artifact retention, and
  the chart-only recovery failure rules.
- Added a concise release-consumption section to `charts/github-webhook-exporter/README.md` with
  the OCI chart coordinate, pull/install examples, and the digest-guarded chart-only recovery rule.

## Why it changed
- Document the immutable image/chart release flow operators must follow after validation passes.
- Keep chart consumption and recovery guidance aligned with the release workflow state machine.

## Verification
- `just workflow-test`
- `git diff --check`
