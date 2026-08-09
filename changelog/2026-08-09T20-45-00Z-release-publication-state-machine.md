# Release publication state machine

## What changed
- Added `scripts/release-publish.sh` to validate canonical release inputs, inspect GHCR image/chart state with fail-closed semantics, and publish only the permitted immutable transitions.
- Added `scripts/release-publish-test.sh` with deterministic fake `docker` and `helm` fixtures covering initial publication, digest-verified chart-only recovery, completed release detection, inconsistent state refusal, digest conflicts, unsupported manifests, inspection failures, and push failures.
- Added `release-publish-test` to `justfile` for focused local verification.

## Why it changed
- Prevent unintended overwrites when release publication is retried after partial success or registry inspection errors.
- Allow the only safe recovery path: republish the missing Helm chart when the remote single-manifest image `config.digest` exactly matches the local Docker image ID.
