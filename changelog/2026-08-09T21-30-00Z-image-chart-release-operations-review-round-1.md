# 2026-08-09T21:30:00Z - Image and chart release operations review round 1

## Summary
- Corrected the release recovery guidance for immutable version tags.
- Replaced the legacy rerun policy with the exact image-existing/chart-missing chart-only recovery rule.
- Kept completed, chart-only, and digest-conflict states fail-closed without overwrite.
- Tightened the workflow documentation contract to require complete policy sentences.

## Files updated
- `docs/operations.md`
- `charts/github-webhook-exporter/README.md`
- `scripts/github-actions-test.sh`

## Verification plan
- `just workflow-test`
- `git diff --check`
