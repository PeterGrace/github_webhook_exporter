# Specification 2: Webhook Ingestion and Prometheus Metrics

## Goal

Accept authenticated GitHub webhooks for configured repositories and expose useful,
bounded-cardinality Prometheus metrics without retaining payloads.

## Dependencies

Specification 1 must be complete.

## Runtime configuration

| Variable | Requirement |
| --- | --- |
| `GHE_WEBHOOK_BODY_LIMIT_BYTES` | Optional positive integer; default and maximum `2097152`. |
| `GHE_DELIVERY_RETENTION_DAYS` | Optional positive integer; default `7`. |
| `GHE_DELIVERY_PRUNE_INTERVAL_SECONDS` | Optional positive integer; default `3600`. |

## HTTP routes

| Method | Route | Success |
| --- | --- | --- |
| `POST` | `/webhooks/github` | `204` for an accepted or duplicate delivery. |
| `GET` | `/metrics` | `200` with Prometheus text exposition. |

Required webhook headers are `Content-Type: application/json`, `X-GitHub-Event`,
`X-GitHub-Delivery`, and `X-Hub-Signature-256`. The delivery header must be a UUID. The signature
must have the exact form `sha256=` followed by 64 hexadecimal characters.

| Status | Meaning |
| --- | --- |
| `204` | Authenticated new or duplicate delivery. |
| `400` | Missing or malformed headers, malformed JSON, or missing/invalid repository identity. |
| `401` | Unknown/disabled repository or invalid signature. |
| `413` | Body exceeds the configured limit. |
| `415` | Content type is not JSON. |
| `503` | SQLite cannot load authentication data or claim the delivery. |

Unknown and disabled repositories are intentionally indistinguishable. Error bodies do not include
repository identities, signatures, delivery IDs, or payload fragments.

## Processing flow

1. Tower enforces the content-type and body limits before JSON parsing.
2. Axum validates headers and retains the exact body bytes.
3. A minimal untrusted parser extracts `repository.full_name` only.
4. The canonical name locates an enabled repository and encrypted secret.
5. The secret is decrypted and HMAC-SHA-256 is calculated over the original bytes.
6. Expected and supplied signatures are compared in constant time.
7. After authentication, atomically insert the delivery UUID into `processed_deliveries`.
8. An existing UUID returns `204` without changing event counters.
9. A new UUID is normalized, counted, and discarded with its payload.

Unverified payload data is used only to locate the candidate secret. No semantic event processing
occurs before authentication.

## Delivery persistence

```sql
CREATE TABLE processed_deliveries (
    delivery_id TEXT PRIMARY KEY,
    received_at TEXT NOT NULL
);

CREATE INDEX processed_deliveries_received_at_idx
    ON processed_deliveries(received_at);
```

The claim uses one `INSERT ... ON CONFLICT DO NOTHING` operation. A background task periodically
deletes expired rows in bounded batches of at most 1,000 until no expired rows remain. Shutdown
cancels the task cleanly.

A crash after a claim commits but before counters update can undercount one delivery. This system
does not promise exactly-once metrics across crashes. During uninterrupted operation, a repeated
delivery UUID is counted once.

## Label normalization

`event_type` is selected from this fixed v1 allowlist:

- `branch_protection_rule`
- `check_run`
- `check_suite`
- `create`
- `delete`
- `deployment`
- `deployment_status`
- `discussion`
- `discussion_comment`
- `issues`
- `issue_comment`
- `merge_group`
- `pull_request`
- `pull_request_review`
- `pull_request_review_comment`
- `push`
- `release`
- `repository`
- `status`
- `workflow_job`
- `workflow_run`
- `other`

The global v1 action allowlist is:

- `assigned`
- `checks_requested`
- `closed`
- `completed`
- `created`
- `deleted`
- `dequeued`
- `destroyed`
- `edited`
- `enqueued`
- `in_progress`
- `labeled`
- `opened`
- `published`
- `queued`
- `reopened`
- `requested`
- `requested_action`
- `rerequested`
- `submitted`
- `synchronize`
- `unassigned`
- `unlabeled`
- `unpublished`
- `updated`
- `waiting`
- `none`
- `other`

An absent action maps to `none`; any present action outside this list maps to `other`. Event and
action normalization is pure and exhaustively tested. Raw header or action values never become
labels.

## Metrics

```text
github_webhook_requests_total{result}
github_webhook_events_total{event_type,action}
github_webhook_processing_duration_seconds{result}
github_webhook_request_body_bytes
github_webhook_duplicates_total
github_webhook_processing_failures_total{stage}
github_repository_configurations
```

Fixed `result` values are `accepted`, `malformed`, `unauthorized`, `too_large`, `unsupported`, and
`unavailable`. Fixed `stage` values for this specification are `authentication`, `delivery_claim`,
`metrics`, and `database`.

Request totals and duration include every webhook request. Event totals and body-size observations
include only authenticated, newly claimed deliveries. `github_repository_configurations` reflects
the current number of configured repository records and is refreshed after successful configuration
mutations and once at startup.

Repository names, delivery IDs, payload fields, signatures, URLs, pull-request numbers, SHAs, and
other attacker-controlled values never appear as labels.

## Logging

Log request outcomes using normalized result and stage fields. Do not log payloads, request bodies,
repository names, delivery IDs, signatures, authorization headers, or decrypted secrets. A database
or internal failure includes an opaque generated error correlation ID that is not used as a metric
label.

## Tests

- HMAC verification uses GitHub-compatible fixtures and proves byte-for-byte sensitivity.
- Router integration tests cover each response status and verify body limits apply before parsing.
- Unknown and disabled repositories produce indistinguishable responses.
- New deliveries increment event metrics once; ordinary redeliveries increment only the duplicate
  metric and request metrics.
- Claim failures return `503` without event metric updates.
- Every unknown event and action maps to `other`; missing actions map to `none`.
- Prometheus output is scanned to prove forbidden values never appear as labels.
- Logs are scanned for payload fragments, repository names, delivery IDs, signatures, and secrets.
- Retention tests use controlled time and verify bounded deletion.

## Acceptance criteria

- Correctly signed webhooks for enabled repositories return `204`.
- Invalid signatures and unknown or disabled repositories return `401`.
- Bodies over 2 MiB return `413` without JSON parsing.
- Ordinary duplicate deliveries do not update event metrics twice.
- Metric label values come only from fixed enumerations.
- No complete payload or secret data is persisted or logged.
- SQLite unavailability during authentication or claiming returns `503`.
- The documented crash boundary makes no exactly-once claim.
