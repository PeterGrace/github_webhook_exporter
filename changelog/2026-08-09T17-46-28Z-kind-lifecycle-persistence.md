# Kind lifecycle persistence acceptance

- Added a disposable Kind harness that builds, loads, installs, probes, diagnoses, and cleans up the
  production image and Helm release.
- Added runtime-only repository configuration and signed pull-request and merge-group transitions.
- Proved repository, delivery-claim, and pull-request queue persistence across pod replacement.
- Proved unavailable OTLP collection produces bounded diagnostics without changing probes or
  authenticated webhook results.
