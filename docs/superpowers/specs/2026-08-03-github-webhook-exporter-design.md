# GitHub Webhook Exporter Design Decomposition

## Purpose

This document replaces the monolithic direction in `docs/build-spec.md` with five ordered,
independently testable specifications. The decomposition prioritizes a secure first release that
accepts authenticated GitHub webhooks and exports bounded-cardinality Prometheus metrics.

## Design decisions

- Deliver the system as vertical capability slices rather than technical layers.
- Treat specifications 1 and 2 as the first release.
- Keep repository administration as an authenticated HTTP API.
- Use separate credentials for API authentication and database encryption.
- Provide best-effort delivery deduplication with an explicit crash boundary rather than claiming
  exactly-once metric updates.
- Map event types and actions through fixed allowlists before using them as metric labels.
- Include structured stderr logs, health endpoints, and graceful shutdown in the first release.
- Defer merge-queue tracking, OTLP export, and Kubernetes packaging to separate specifications.

## Dependency graph

```text
1. Service and repository configuration
   └── 2. Webhook ingestion and Prometheus metrics
       └── 3. Merge-queue tracking

1–2 ──> 4. OTLP observability
1–4 ──> 5. Kubernetes packaging
```

Specifications 3 and 4 may be developed independently after specification 2. Specification 5
integrates the operational requirements of all preceding capabilities.

## Component boundaries

The runtime uses shared application state containing focused components:

- `RepositoryStore`: repository metadata and encrypted-secret persistence.
- `SecretCipher`: repository-secret encryption and decryption.
- `AdminAuthenticator`: configuration API bearer-token authentication.
- `WebhookAuthenticator`: GitHub HMAC verification over original request bytes.
- `DeliveryStore`: delivery claims and retention.
- `Metrics`: fixed-label Prometheus instruments.

Later specifications add queue processing and telemetry behind separate interfaces. Neither may
alter webhook authentication or acceptance semantics.

## End-to-end webhook flow

1. Reject unsupported content types and bodies over 2 MiB.
2. Validate required headers while retaining the original body bytes.
3. Minimally deserialize `repository.full_name` from the untrusted payload.
4. Load the enabled repository and decrypt its candidate webhook secret.
5. Verify `X-Hub-Signature-256` over the exact original bytes.
6. Claim the validated delivery UUID in SQLite.
7. Return `204` immediately for an existing claim.
8. Map event and action values through fixed allowlists, update metrics, and return `204`.
9. Drop body bytes and decrypted secret without retaining them.

A crash after claiming a delivery but before incrementing metrics can undercount that delivery.
This is the documented limit of v1 deduplication.

## Security model

`GHE_MASTER_KEY` is a base64-encoded 32-byte encryption root key. A versioned repository-secret
key is derived using HKDF-SHA-256 with a fixed application context. `GHE_ADMIN_TOKEN` is an
independent high-entropy bearer token. It is never used for encryption, and the master key is never
sent over HTTP.

Repository secrets use authenticated encryption with a fresh nonce per write. Payloads,
signatures, plaintext secrets, authorization headers, repository identities, pull-request numbers,
and commit identifiers are excluded from logs, traces, and metric labels unless a later
specification explicitly permits a bounded representation.

## Verification strategy

Integration tests exercise behavior through the complete Axum router whenever middleware,
authentication, persistence, and metrics interact. Unit tests cover pure validation,
normalization, cryptography, and state transitions. Each specification defines its own acceptance
criteria and must pass formatting, compilation, Clippy with warnings denied, and all tests before
the next dependent specification begins.

## Source specifications

1. [`2026-08-03-01-service-and-repository-configuration.md`](2026-08-03-01-service-and-repository-configuration.md)
2. [`2026-08-03-02-webhook-ingestion-and-metrics.md`](2026-08-03-02-webhook-ingestion-and-metrics.md)
3. [`2026-08-03-03-merge-queue-tracking.md`](2026-08-03-03-merge-queue-tracking.md)
4. [`2026-08-03-04-otlp-observability.md`](2026-08-03-04-otlp-observability.md)
5. [`2026-08-03-05-kubernetes-packaging.md`](2026-08-03-05-kubernetes-packaging.md)

`docs/build-spec.md` remains historical input. Where it conflicts with these documents, the scoped
specifications govern.
