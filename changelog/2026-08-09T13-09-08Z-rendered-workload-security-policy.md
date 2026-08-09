# Rendered workload security policy

- Added Conftest/Rego workload policy for StatefulSet renders with stable deny IDs GWE001 through GWE012.
- Added one negative StatefulSet fixture per stable policy ID and a mapping contract file.
- Added a `just helm-policy` recipe and a focused policy harness that validates the supported render matrix first.
- Kept the chart render matrix and schema checks unchanged.
