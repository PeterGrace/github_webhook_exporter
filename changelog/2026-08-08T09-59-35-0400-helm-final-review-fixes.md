# Helm final-review fixes

- Removed Secret values from `kubectl` process arguments in the chart installation example and Kind
  acceptance script. Both now use mode-restricted temporary files, `--from-file`, and cleanup traps.
- Added a deterministic rendered-ConfigMap checksum to the StatefulSet pod template so non-secret
  configuration changes trigger the configured `RollingUpdate` behavior.
- Made `service.port` authoritative for the Service, container, probes, and IPv6 wildcard
  application listener, removing the redundant `application.bindAddress` chart value.
- Bounded application shutdown at 300 seconds, telemetry shutdown at 120 seconds, termination grace
  at 600 seconds, probe delays and periods at 300 seconds, probe timeouts at 60 seconds, and probe
  failure thresholds at 10.
- Tightened and reused Kubernetes quantity validation so malformed non-empty PVC and CPU/memory
  resource quantities are rejected.
- Added focused render boundaries, cross-field diagnostic checks, ConfigMap checksum regressions,
  non-default port coverage, Secret-argument isolation coverage, and the declared `cat` dependency.
- Documented the 46-value chart surface, supported ranges, single-port contract, and the storage
  handoff implications of checksum-triggered StatefulSet rollouts.
