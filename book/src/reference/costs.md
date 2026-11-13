# CI cost price table

The optional `costs` table holds the unit prices that later cost-attribution work uses to price CI
jobs, Daytona sandbox time, and per-run vendor charges. Workflows never send rates, so every price
comes from deployment config. When the table is absent, every cost feature is off.

## Supplying the table

Set `costs` in Helm values. The chart renders it as compact JSON into one environment variable,
`GHE_COSTS_JSON`, and leaves the variable out entirely when `costs` is empty. Without Helm, set
`GHE_COSTS_JSON` to the same JSON yourself.

```yaml
costs:
  runners:
    default_usd_per_minute: 0.008
    labels:
      ubuntu-latest-m: 0.016
      ubuntu-24.04-xl: 0.064
  daytona:
    usd_per_cpu_second: 0.00001
    usd_per_ram_gb_second: 0.000005
  vendors:
    example-review-bot:
      match: { review_login: "review-bot[bot]" }
      usd_per_run: 0.50
    example-visual-tests:
      match: { check_suite_app: "visual-tests" }
      usd_per_run: 0.25
```

The values above are placeholders, not real prices.

Put prices in a values file, or pass them with `--set-json`. Helm's `--set` passes decimals such
as `0.01` as strings, and the schema rejects them.

| Field | Required | Contract |
| --- | --- | --- |
| `runners.default_usd_per_minute` | yes | USD per billable minute for a job whose labels match no entry in `runners.labels`. |
| `runners.labels` | no | Map of runner label to USD per billable minute. |
| `daytona.usd_per_cpu_second`, `daytona.usd_per_ram_gb_second` | no; both or neither | Daytona sandbox rates. |
| `vendors.<name>.match` | yes, per vendor | Exactly one of `check_suite_app` (a GitHub App slug) or `review_login` (a reviewer login), non-empty. |
| `vendors.<name>.usd_per_run` | yes, per vendor | USD charged for one run. |

Every rate must be a finite number of at least 0.

## Validation

The service parses the table once at startup and refuses to start when it is invalid. The table is
invalid when:

- the JSON is malformed or has an unknown field;
- a rate is negative or not a number;
- a runner label or vendor name is empty;
- a vendor has no match rule, two match rules, or an empty one;
- two runner labels, two app slugs, or two reviewer logins differ only in case, or two vendors
  claim the same app slug or the same reviewer login.

The error names `GHE_COSTS_JSON` and never includes its value. The Helm chart schema enforces the
same rules at render time, except the checks that compare keys with each other, so most mistakes
fail at `helm template` or `helm upgrade` before a pod starts.

Unknown fields are rejected on both sides. If a later release adds a field to `costs`, roll out
that release before adding the field to your values.

Prices are read once. Changing them requires a restart, which `helm upgrade` performs when the
ConfigMap changes.

## Lookups

| Lookup | Result |
| --- | --- |
| Runner rate for a job | The rate of the first of the job's labels, in job order, found in `runners.labels`. Otherwise `runners.default_usd_per_minute`, marked as a fallback. |
| Daytona rates | The CPU-second and RAM-GB-second rates, or nothing when `daytona` is absent. |
| Vendor by check-suite app | The vendor whose `match.check_suite_app` equals the app slug. |
| Vendor by reviewer login | The vendor whose `match.review_login` equals the login. |

Runner labels, app slugs and reviewer logins match regardless of ASCII case, as GitHub compares
them. The table stores them lowercased.

Each completed job is priced at its billable minutes times its runner rate. The job span then
carries `ci.cost_usd`, the rate as `ci.price.usd_per_minute`, and the matched label as
`ci.price.label`, and the exporter writes a `ci cost recorded` log; see
[Runner and cost attributes](traces.md#runner-and-cost-attributes). A job that billed 0 minutes,
such as a skipped job, is priced at 0 without a rate lookup.

A job whose labels have no price is charged at `runners.default_usd_per_minute`. Self-hosted
runners are included: price their label, for example `self-hosted: 0`, if their minutes should
cost nothing.

A runner fallback logs one `ci price missing` warning when `runners.labels` has at least one
entry. A table with no `labels` is a deliberate flat rate, so falling back to
`default_usd_per_minute` logs nothing. It is a parentless log on the default
target, so it is exported with other OTLP logs. Its fields are:

| Field | Contents |
| --- | --- |
| `labels` | The job's labels: at most 8, each cut to 64 characters. |
| `labels_truncated` | `true` when the job had more than 8 labels, or any logged label was cut. |
| `default_usd_per_minute` | The rate used instead. |
| `price_table_version` | The table version. |

Runner labels come from each workflow's `runs-on`. They identify runner types, not people or
credentials, and they are logged on purpose so you can see which label needs a price.

## Version

`ci.price_table.version` is the first 12 hex characters of the SHA-256 digest of the table's
canonical form. The canonical form is the parsed table, with labels, slugs and logins lowercased,
re-serialized as compact JSON with struct fields in a fixed order and map keys sorted. Key order,
whitespace, the case of labels, slugs and logins, and number spelling (`0.5` compared with `5e-1`,
or `-0` compared with `0`) don't change the version. Changing any rate, label, or vendor does.
Cost records carry this version so a figure can be traced back to the rates that produced it.
