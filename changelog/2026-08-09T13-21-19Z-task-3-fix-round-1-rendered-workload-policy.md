# Task 3 fix round 1: rendered workload policy

- Centralized workload container selection so GWE002-GWE010 apply to both ordinary containers and initContainers.
- Rejected the deprecated `serviceAccount` alias in addition to `serviceAccountName` for GWE012.
- Added focused negative fixtures for a privileged initContainer and a serviceAccount alias.
- Preserved the supported Helm render matrix and stable policy IDs.
