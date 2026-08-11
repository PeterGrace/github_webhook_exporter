# Grafana repository filter design amendment

- Merged the latest `main`, including repository-scoped webhook, merge-queue, and workflow metrics.
- Amended the dashboard design and implementation plan with a dependent, multi-value Repository
  variable.
- Defined repository filtering for labelled metric families while preserving exporter-global
  configured-repository and telemetry panels.
- Retained the synthetic `unknown` repository value so pre-authentication outcomes remain
  inspectable.
- Expanded the validation contract to distinguish repository-scoped and global PromQL queries.
