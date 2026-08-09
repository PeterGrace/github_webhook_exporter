# Helm packaging secret scan and archive regression checks

- Added category-safe secret fixtures and contract mapping.
- Implemented `helm-secret-scan.sh` with `--test` coverage for source, values, rendered manifests, and negative fixtures.
- Implemented `helm-package-test.sh` to package, inspect, safely extract, and revalidate the chart archive.
- Added `helm-secrets` and `helm-package` just recipes and ignored `dist/`.
