# Load, validate, and version an optional costs price table

Date: 2026-11-11T16:50:05Z

## What changed

- New `src/costs.rs` holds `CostsConfig`: a unit-price table for runners (USD per billable
  minute, by label with a default), Daytona sandboxes (USD per CPU-second and per RAM-GB-second),
  and per-run vendors matched by check-suite app slug or reviewer login.
- `RuntimeConfig::from_lookup` reads it from `GHE_COSTS_JSON`. Unset means `costs()` is
  `None` and every cost feature is off. Set but invalid fails startup with
  `ConfigError::Invalid { variable: "GHE_COSTS_JSON" }`. Parser messages are dropped because
  serde can quote configured values.
- Validation is mostly done by types. `UsdRate` deserializes through `TryFrom<f64>`, so a
  negative or non-finite rate cannot be represented. `-0.0` is normalized to `0.0` so it
  cannot change the version hash. The vendor `match` is an externally tagged
  enum, which accepts exactly one key. `deny_unknown_fields` rejects typos. Empty match strings,
  empty runner labels and vendor names, and two vendors claiming the same app slug or login, are
  rejected while building the lookup indexes.
- Runner labels, app slugs and reviewer logins are lowercased (ASCII) at load and at lookup,
  because GitHub compares them case-insensitively. Keys that collide after lowercasing are
  rejected. Lowercasing happens before hashing, so key case does not change the version. Lookups
  borrow already-lowercase input instead of allocating.
- `CostsConfig::version()` is `ci.price_table.version`: the first 12 hex characters of the
  SHA-256 digest of the parsed table re-serialized as compact JSON. Fixed field order and
  `BTreeMap` keys make that form canonical.
- Lookups: `runner_rate` (first job label found, else the default, reporting whether it fell back),
  `daytona_rates`, `vendor_for_check_suite_app`, and `vendor_for_review_login`. Vendor lookups
  use `HashMap` indexes built once at startup.
- A runner fallback emits one parentless `ci price missing` warning, only when `runners.labels`
  is non-empty (an empty map is a deliberate flat rate). The warning goes to the default
  (exported) target, with `labels` (at most `MAX_LOGGED_LABELS` = 8, each cut to
  `MAX_LOGGED_LABEL_CHARS` = 64 characters), `labels_truncated` (either bound applied),
  `default_usd_per_minute`, and `price_table_version`.
- `RuntimeConfig` `Debug` output reports `costs_enabled` and `costs_version`.
- Helm: `costs: {}` in `values.yaml`, a matching schema (non-negative numbers, exactly one match
  rule per vendor, non-empty match strings, runner labels and vendor names, `runners` required
  once `costs` is non-empty, no unknown fields), and
  `GHE_COSTS_JSON` rendered by the ConfigMap only when `costs` is non-empty.
  `scripts/helm-chart-test.sh` covers the absent and present renders and the schema rejections.
- Docs: a new book page `reference/costs.md`, plus entries in the environment-variable,
  Helm-values, and chart README references.

## Why

Issue #126 (release R1 of CI cost attribution). Workflows report quantities and inference
dollars but never runner rates, so all unit prices come from deployment config. Cost records
carry the table version so each figure traces back to the rates that produced it.

## Decisions

- **Env var, not a mounted file.** The service's config is env-only (`RuntimeConfig::from_lookup`),
  and the chart already projects non-secret config through the ConfigMap. One JSON variable keeps
  that model and adds no YAML dependency. `GHE_COSTS_PATH` was not added.
- **Labels in the fallback warning, exported.** Labels come from workflow `runs-on` values. They
  name runner types, not people or credentials. Logging them intentionally shows which label
  needs a price. The bounds stop an unusual workflow from producing a very large log line.
- **`runners` required, `daytona` and `vendors` optional.** A deployment without Daytona or
  vendors omits those blocks instead of writing zero rates.
- **Schema strictness has an upgrade cost.** Unknown fields fail in both the schema and the
  binary, so a future field must ship in the image before it appears in values.

## Out of scope

No cost is computed or emitted yet. Nothing reads job labels, app slugs, or reviewer logins from
webhooks. The table is read once at startup.
