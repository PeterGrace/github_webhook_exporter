# Fix Kind Repository Metric Assertions

Updated the Helm Kind lifecycle acceptance harness to assert the repository-scoped Prometheus contract introduced by issue #64.

## Changes

- Added `repository="acceptance/repository"` to webhook event, duplicate, merge-queue outcome, and merge-queue duration assertions.
- Reused the lifecycle test's canonical `REPOSITORY_NAME` value so the expected series remains aligned with the configured authenticated repository.
- Left the process-wide repository configuration metric assertion unlabeled.

## Root cause

The exporter emitted the new repository label correctly, but the Kind acceptance script still searched for the previous unlabeled samples. Its fixed-string match therefore rejected valid Prometheus exposition.
