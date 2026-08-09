# Task 5: Pinned CI tool installation and GitHub Actions workflow

- Added `ci/tool-versions.env` with pinned tool versions and checksums.
- Added `scripts/install-ci-tools.sh` to verify, install, and report immutable CI tools.
- Added `scripts/github-actions-test.sh` to parse the workflow safely and enforce trigger, pinning, ordering, and artifact contracts.
- Added `.github/workflows/helm-package-ci.yml` with pinned actions, least-privilege permissions, sequential validation, and artifact upload.
- Added `workflow-test` to `justfile`.
- Tightened existing shell scripts so ShellCheck passes across tracked shell files.
