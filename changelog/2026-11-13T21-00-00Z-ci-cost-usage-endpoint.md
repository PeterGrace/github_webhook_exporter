# Accept OIDC-authenticated in-job usage reports

Date: 2026-11-13T21:00:00Z

## What changed

- New optional `POST /ci-cost/usage`. A GitHub Actions job reports inference spend (already priced
  in USD) and Daytona sandbox seconds, authenticated by the job's GitHub Actions OIDC token. All
  attribution comes from the verified claims: repository, workflow, workflow file, reusable
  workflow file, run, attempt, event and SHA. Unknown body fields fail, so a body cannot override
  them.
- Configure it with `GHE_INGEST_OIDC_JSON`, or `ingest.oidc` in Helm values: an audience and a list
  of allowlisted repositories. The repository must also be registered and enabled. When unset, the
  route is not registered and returns 404. The chart routes `/ci-cost/usage` through the webhook
  Ingress when configured.
- Tokens must be RS256, signed by a key in GitHub's published JWKS, issued by
  `https://token.actions.githubusercontent.com`, for the configured audience and unexpired. Keys are
  cached for an hour. At most one JWKS fetch is attempted a minute, successful or not, with
  redirects disabled and a 64 KiB response cap. While the last attempt failed, a token whose key is
  not cached gets a retryable `503 jwks_unavailable`. The token is never logged or printed.
- The first accepted token for a registered repository pins it to the token's `repository_id`, GitHub's
  immutable numeric ID. A later token for the same name with a different ID, such as one from a
  repository recreated under a freed name, gets `403 repository_id_mismatch`. Deleting and
  re-registering the repository through the admin API clears the pin.
- Daytona entries are priced from `costs.daytona`. Without those rates they are recorded at 0 USD
  with `ci.price.fell_back=true`.
- Each accepted entry is stored in the new `ci_usage_entries` table, and pins in
  `ci_usage_repository_pins` (both migration `202611130002`). Each entry is
  exported as one `ci.usage` root span, and logged as one `ci cost recorded` info log. The PR comes
  from the run's stored `workflow_run` context when it has arrived.
- Each rejection returns a fixed JSON error and writes one `ci usage post rejected` warning with a
  closed `reason`.
- A busy or locked database returns a retryable `503 storage_unavailable`. Any other storage
  failure returns `500 internal_error` with an `error_id` that matches one `ci usage post failed`
  error log.
- The issuer is fixed to GitHub.com's. GitHub Enterprise Server and customized enterprise issuers
  are not supported yet.
- Usage entries are pruned on the processed-delivery retention cutoff (`GHE_DELIVERY_RETENTION_DAYS`,
  7 days by default).

## Upgrade note

With NetworkPolicy egress enabled, the pod must reach `token.actions.githubusercontent.com:443` for
the endpoint to verify tokens.
