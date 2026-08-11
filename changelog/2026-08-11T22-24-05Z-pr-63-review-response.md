# PR 63 review response

- Added explicit `lastNotNull` reducers to every Grafana stat panel so displayed values do not
  depend on Grafana defaults.
- Added metric-specific PromQL grouping-label contract checks for webhook, merge-queue, workflow,
  and telemetry panels.
- Retained semantic rate units: requests use `reqps`, events use `eps`, and generic failures,
  rejections, and telemetry records use `ops`.
- Synced the branch with the latest `main` before applying review changes.
