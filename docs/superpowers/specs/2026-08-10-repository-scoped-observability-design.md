# Repository-Scoped Observability Design

## Problem

The exporter accepts webhooks for multiple configured GitHub repositories, but its Prometheus
series aggregate repository-scoped activity across all repositories. Operators therefore cannot
compare webhook, merge-queue, or workflow behavior per repository. Existing OpenTelemetry child
spans generally include repository identity, but the root HTTP span does not receive that identity.

## Repository identity

Repository-specific telemetry will use the canonical GitHub full name in `{owner}/{repository}`
form, exposed as the Prometheus label `repository` and the existing OpenTelemetry attribute
`github.repository.name`. For example, the repository is
`PeterGrace/github-webhook-exporter`, not `github-webhook-exporter`.

The existing `CanonicalRepositoryName` validation remains authoritative. Its canonical lowercase
representation is used consistently, so the example above is emitted as
`petergrace/github-webhook-exporter` after normalization.

Repository identity becomes trusted only after webhook authentication succeeds. Requests that fail
before authentication use the fixed Prometheus value `repository="unknown"`; they do not copy an
attacker-controlled payload value into metrics or traces. This bounds unauthenticated cardinality
and prevents repository spoofing.

## Prometheus behavior

Add `repository` to every metric whose observation describes one webhook repository:

- `github_webhook_requests_total{repository,result}`
- `github_webhook_events_total{repository,event_type,action}`
- `github_webhook_processing_duration_seconds{repository,result}`
- `github_webhook_request_body_bytes{repository}`
- `github_webhook_duplicates_total{repository}`
- `github_webhook_processing_failures_total{repository,stage}`
- `github_merge_group_events_total{repository,action,reason}`
- `github_merge_queue_pr_outcomes_total{repository,outcome,reason}`
- `github_merge_queue_attempt_duration_seconds{repository,outcome}`
- `github_merge_queue_transition_failures_total{repository,reason}`
- `github_workflow_job_steps{repository}`
- `github_workflow_job_trace_rejections_total{repository,reason}`

The following process-wide metrics remain unchanged because they do not describe one webhook
repository:

- `github_repository_configurations`
- `github_telemetry_export_failures_total{signal,reason}`
- `github_telemetry_dropped_records_total{signal,reason}`

Existing zero-valued startup series for repository-scoped metric families use
`repository="unknown"`. Authenticated repository series are created lazily when observed.
Configured repository names already impose the accepted cardinality for authenticated series.

## Request context and data flow

The outer HTTP middleware creates a cloneable request-local repository context before routing and
places it in request extensions. The webhook handler receives the same context and sets it exactly
once after successful authentication, using the canonical full repository name.

Repository-scoped metric call sites accept the canonical repository name explicitly when they
already execute after authentication. The webhook request middleware reads the shared context after
the response completes, allowing it to label successful and post-authentication failure outcomes.
If the context was never set, it records the fixed `unknown` value.

The root `http.request` span reads the same context after the inner service completes and adds
`github.repository.name` when authentication succeeded. Existing authentication, processing,
merge-queue, and historical workflow spans continue using the same canonical attribute. No
repository attribute is added to failed pre-authentication traces.

## Security and cardinality

- Never derive a metric label or trace repository attribute from an unauthenticated payload alone.
- Never use raw repository spelling after canonical validation.
- Use one fixed `unknown` fallback for malformed, oversized, unsupported, unknown-repository,
  invalid-signature, and pre-authentication dependency-failure requests.
- Do not add delivery IDs, pull-request numbers, SHAs, secrets, payloads, or headers to metrics.
- Continue excluding span-only identifiers from local formatted span output.

## Testing

Tests will prove that:

1. Two authenticated repositories create distinct Prometheus series using full canonical
   `owner/repository` names.
2. Repository-scoped counters and histograms include the new label, including specialized
   merge-queue and workflow instruments.
3. Process-wide repository-count and telemetry-diagnostic metrics remain unlabeled by repository.
4. Pre-authentication failures use only `repository="unknown"` and cannot create arbitrary series.
5. The root HTTP span and existing repository-specific child spans contain
   `github.repository.name` with the full canonical name after authentication.
6. Existing duplicate, bounded-vocabulary, redaction, and concurrency behavior remains intact.

Validation uses the project gates: `cargo fmt --check`, `cargo build`,
`cargo clippy --all-targets -- -D warnings`, and `cargo test`.

## Documentation

Update the governing observability documentation to describe repository-scoped cardinality, the
`unknown` fallback, full-name formatting, and trace propagation. Add a timestamped changelog entry
for the implementation iteration.

## Out of scope

- Adding repository labels to process-wide configuration or exporter-health metrics.
- Adding repository identity to requests unrelated to GitHub webhook ingestion.
- Changing repository canonicalization or allowing display-case preservation in telemetry.
- Adding any new configuration flag or dependency.
