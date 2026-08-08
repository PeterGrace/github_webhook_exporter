# Helm PDB and security boundary coverage

- Added an optional `policy/v1` PodDisruptionBudget with fixed `minAvailable: 0` semantics and the StatefulSet's stable selector labels.
- Added installation notes limited to Service discovery, health paths, the existing Secret name, and the mandatory singleton reminder.
- Expanded chart tests for disruption defaults, shutdown and telemetry boundaries, required Secret references, probe and port validation, and rendered credential hygiene.
