# Issue 54 final validation

## Focused release contracts

All focused commands exited 0:

- `just release-version-test` printed `release version tests passed`.
- `just release-publish-test` printed `release publication tests passed`.
- `just workflow-test` validated `.github/workflows/helm-package-ci.yml` without errors.
- `scripts/release-version.sh v0.1.0` printed only `0.1.0`.

## Delivered artifact validation

All artifact commands exited 0:

- `just helm-static` linted and tested the chart, rendered all 10 supported scenarios,
  validated 38 resources against Kubernetes 1.31.0 and 1.35.0 schemas, passed workload policy
  checks and secret scans, rejected the negative fixtures, and produced the validated archive
  `dist/github-webhook-exporter-0.1.0.tgz`.
- `helm show chart ./dist/github-webhook-exporter-0.1.0.tgz` reported chart and application
  version `0.1.0` with Kubernetes support `>=1.31.0-0 <1.36.0-0`.
- The exact archive render command exited 0 without output:

  ```bash
  helm template archive ./dist/github-webhook-exporter-0.1.0.tgz --kube-version 1.35.0 >/dev/null
  ```

- `just image-smoke` built the production image for `linux/amd64` and reported
  `container smoke checks passed for github-webhook-exporter:dev`.

The installed Helm version requires the explicit `./dist/...` local path; this is equivalent to
using the task's local archive and prevents Helm from parsing `dist/...` as a repository reference.
No GHCR authentication or publication was performed.

Additional local artifact inspection recorded:

- `sha256sum ./dist/github-webhook-exporter-0.1.0.tgz` returned
  `b0d682c5d9c731708d96f98a79a8f299557514dd834293ff675950e81859727d`.
- The following inspection command reported image ID
  `sha256:3cd631ce36fa1add69a64dcffae070803e20b116176fe83418055066e7a800b0`,
  `linux/amd64`, user `65532:65532`, and entrypoint
  `["/usr/local/bin/github_webhook_exporter"]`:

  ```bash
  docker image inspect github-webhook-exporter:dev --format 'Id={{.Id}} Architecture={{.Architecture}} Os={{.Os}} User={{.Config.User}} Entrypoint={{json .Config.Entrypoint}}'
  ```

## Mandatory project gates

The mandatory sequence was run once, in the required order, and every command exited 0:

1. `just fmt` passed `cargo fmt --all -- --check`.
2. `cargo clippy --all-targets -- -D warnings` completed without warnings.
3. `just test` passed all 244 tests across the library and integration test targets with zero
   failures.

No mandatory gate failed, so no fix or sequence restart was required.

## Extended gates

Every extended command exited 0:

- `cargo build --locked` completed without warnings.
- `cargo doc --no-deps --locked` generated the crate documentation without warnings.
- The exact two-command ShellCheck gate passed all tracked shell files without output:

  ```bash
  mapfile -t shell_files < <(git ls-files -- '*.sh')
  shellcheck "${shell_files[@]}"
  ```

- `git diff --check` produced no output.
- `git status --short` produced no output because the working tree was clean immediately before
  this final evidence file was created. A post-creation status check then listed only
  `changelog/2026-08-09T21-30-00Z-issue-54-final-validation.md` as untracked.

## Outcome

All focused, artifact, mandatory, and extended local validation gates passed. The validation did
not authenticate to or publish anything to GHCR. Step 7 delivery is intentionally deferred until
the required whole-branch review is complete.
