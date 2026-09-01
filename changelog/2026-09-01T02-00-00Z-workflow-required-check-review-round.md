# PR #98 review round: refresher seam, log target, and a path-safety note

**Issue:** [#95](https://github.com/PeterGrace/github_webhook_exporter/issues/95)
**Follows:** [2026-09-01T01-00-00Z-workflow-required-check-helm-chart.md](2026-09-01T01-00-00Z-workflow-required-check-helm-chart.md)

Three review comments on PR #98, all addressed in-branch rather than deferred.

## `RequiredCheckSource` — the seam that was missing

`src/github/refresh.rs` shipped with no tests, and the reason was structural: `RequiredCheckRefresher`
owned a concrete `GitHubAppClient`, so its orchestration could not be driven without a network and
an RSA private key. No test file would have fixed that.

There is now a one-method `RequiredCheckSource` trait, implemented by `GitHubAppClient` and held as
`Box<dyn RequiredCheckSource>`. **Boxed deliberately, not generic:** `RequiredCheckRefresher` is part
of the public `BackgroundServices` surface, so a type parameter would propagate out through
`src/app.rs` and `src/main.rs` for no benefit. One virtual call per refresh is irrelevant at a rate
bounded by the cache TTL.

Eight tests now cover all four `refresh()` branches, the queue-full `try_send` drop, and both
`run()` exits. Two were written specifically so they cannot pass vacuously, since a test that only
appears to check something is worse than no test:

- The dedup test asserts the source is called **exactly once across five requests**, and was
  confirmed to fail with `left: 5, right: 1` when `Ok(Some(_)) => return` is deleted. The claim "a
  burst of deliveries for one branch costs one API read" was previously asserted only in prose, in
  the first changelog entry of this series — it is now a property with a test behind it.
- The cache-write test asserts **zero rows** in `required_check_contexts` afterward, not merely that
  the fetch happened, so it can only pass if the upsert genuinely failed.

The fetch-failure test asserts that a `Forbidden` leaves nothing cached, locking in the rule that a
permission problem is never stored as "not required".

## A log that crossed the local-only boundary

`RequiredCheckRefreshHandle::request` is the one function in the refresh module that runs inline in
the webhook handler; every other log there is on the background task and already carried
`parent: None`. Its `debug!` had neither `parent: None` nor `LOCAL_ONLY_LOG_TARGET`, so a dropped
refresh was admitted to exported logs and would attach to the live request trace — while the sibling
log in `webhook.rs` for the same condition already used the local-only target.

Nothing leaked: the payload is a fixed `outcome = "queue_full"` with no request data. Fixed anyway,
because a convention that holds everywhere except one place is not a convention. The site carries a
comment explaining why this call needs both markers stated explicitly while the rest of the module
inherits them, so the asymmetry does not later read as an accident.

## What actually bounds branch-path injection

`BRANCH_SEGMENT_ENCODE_SET` leaves `/` literal, which is required for `gh-readonly-queue/main/pr-7`
style refs. That means a branch name from a webhook payload can add path segments to the request
target, and the encode set is therefore *not* the control that makes this safe.

The set is unchanged; its documentation now names the three controls that do the work — the
installation-scoped token capping an injected path at what the App may already read, the
`http`/`https` base URL validated at configuration time closing any cross-host reach, and Git's own
ban on `..` in ref names closing traversal — and states that tightening the set would be defense in
depth. This is the kind of reasoning that is expensive to reconstruct during a later audit and cheap
to write down once.
