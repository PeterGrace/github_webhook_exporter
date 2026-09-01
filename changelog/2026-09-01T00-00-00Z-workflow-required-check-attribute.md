# Branch-protection required-check attribute on workflow spans

**Issue:** [#95](https://github.com/PeterGrace/github_webhook_exporter/issues/95)

## What shipped

Workflow job spans now carry `github.workflow.required`, a tri-state boolean reporting whether the
job is a branch-protection required status check on its run's target branch. Steps inherit the job's
value. The answer comes from a local SQLite cache filled out-of-band by a GitHub App-authenticated
client; the synchronous webhook path only ever reads that cache.

Three pieces, in the order the groomed issue sequenced them:

1. **Tri-state plumbing.** `WorkflowJobTrace` gained `required: Option<bool>`, threaded through
   `WorkflowJobTraceParts`. `workflow_required_attribute` in `src/telemetry/trace.rs` returns
   `Option<KeyValue>` under the new `github.workflow.required` key, so unknown emits nothing at all.
2. **`RequiredCheckStore`.** A cache keyed by `(repository_id, target_branch)` holding the
   sanitized required-check names and a timestamp, with read-time TTL expiry and batched pruning
   through the existing retention loop.
3. **GitHub App fetcher.** A background refresher that mints short-lived installation tokens and
   reads `/repos/{owner}/{repo}/branches/{branch}/protection/required_status_checks`.

## Decisions worth recording

**Unknown is absence, not `false`.** The attribute is omitted entirely when the cache holds no fresh
answer. Emitting `false` would make "we never looked" indistinguishable from "GitHub says this is
optional", and a dashboard would silently mis-bucket every cold start. Consumers must treat a
missing attribute as its own state.

**The hot path never calls GitHub.** Trace emission runs inline in the webhook handler before the
`204 No Content` is written. An inline lookup — even one guarded by a cache — would put GitHub's
availability in front of every acknowledgement, and a GitHub incident would surface here as
redelivered or dropped webhooks. So a miss queues a refresh on a bounded channel with `try_send`,
reports unknown, and returns. A full queue drops the request rather than applying backpressure to an
inbound delivery.

**Only `Some(false)` suppresses parent-failure reporting.** This closes the reporter's original
"only mark the parent as failed if required is true" request, but deliberately not literally:
`Some(true)` *and* `None` both preserve today's behavior. With a five-minute TTL the first delivery
per branch after expiry is legitimately unknown, so treating unknown as "suppress" would silently
stop flagging genuine failures. Step spans are never suppressed — they remain the factual record of
what failed, so a suppressed job can still have an error child.

**GitHub App over PAT, with the private key from a file or base64.** App installation tokens are
short-lived and per-installation scoped, a smaller blast radius than a long-lived PAT in a service
that otherwise holds only a local master key and admin token. `GHE_GITHUB_APP_PRIVATE_KEY_PATH`
takes precedence over `GHE_GITHUB_APP_PRIVATE_KEY` so a mounted Kubernetes secret is never shadowed
by a stale environment value. A partial App configuration is fatal at startup rather than a silent
fallback to disabled, which would otherwise present as "everything is unknown" with no explanation.

**`404` is an answer, not an error.** GitHub returns `404` for an unprotected branch. That is a
confident empty required-check set, so jobs on unprotected branches report `required=false` instead
of unknown. `401`, `403`, `429`, and other statuses map to distinct bounded errors and leave the
branch unknown, so a permission problem never gets cached as "not required".

**Newline-joined name storage.** `DisplayName::sanitize` strips every Unicode control character, so
a newline cannot occur inside a check name. That makes newline-joining a lossless, injection-free
encoding and avoids a JSON blob in the cache column. Names are re-sanitized on read, so a row
written by an older build or edited out of band cannot smuggle an unbounded name into a span.

## Incidental refactors

- `wait_for_cancellation` moved to `src/lifecycle.rs`; retention, the refresher, and both
  `src/app.rs` shutdown futures had been carrying four copies of the same loop.
- `PrunableStore` became object-safe (via `async_trait`, already a dependency) so a retention pass
  iterates a workload list instead of repeating a sequence-and-check block per store. Adding the
  fifth store would otherwise have pushed `prune_retention_workloads` to eight parameters.
- `serve_with_shutdown`'s fifth parameter became `BackgroundServices`, grouping the retention
  configuration with the optional refresher rather than growing the signature.

## Helm chart

Deliberately not part of this change, and delivered immediately after it in
[2026-09-01T01-00-00Z-workflow-required-check-helm-chart.md](2026-09-01T01-00-00Z-workflow-required-check-helm-chart.md).
The chart has its own validation suite and its own decisions — how the PEM reaches the pod, and what
egress a NetworkPolicy must now allow — so it was worth separating from the application change
rather than folding in.
