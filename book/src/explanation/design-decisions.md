# Design decisions

A handful of decisions recur throughout the reference material without their reasoning spelled
out in one place. This page collects the "why" behind them.

## Why webhook payloads are never persisted

GitHub webhook payloads routinely contain commit messages, PR titles and descriptions, actor
identities, and sometimes more than that — none of which this service has a reason to retain to do
its job of counting events and tracking queue state. Not persisting them isn't an optimization
applied after the fact; it shapes the request path directly. The service reads only the
`repository.full_name` field and, for a few specialized event types, a small number of typed
fields (a PR number, a merge state, a workflow job's timing and conclusion) needed to update
bounded state — the rest of the body is verified against the HMAC signature and then discarded.
That's a smaller attack surface (nothing sensitive to leak from a database compromise or a log
line), a smaller compliance surface (nothing to purge on request, because nothing was kept), and a
forcing function against scope creep — a feature request that requires holding onto payload
content is, by construction, a bigger change than adding one more bounded counter.

## Bounded cardinality and span-only identifiers

Every Prometheus label set in this service is a closed vocabulary: event types, actions, results,
and reasons all collapse unknown or unexpected values to `other` rather than admitting them as new
label values. High-cardinality identifiers — repository names beyond the label already used
per-metric, delivery IDs, PR numbers, commit SHAs, workflow/job/step IDs — never become metric
labels at all. The reasoning is entirely about protecting the metrics backend: an unbounded label
value is effectively attacker- or GitHub-controlled cardinality growth, and a busy organization
across enough repositories and event types could otherwise turn `/metrics` into an unbounded time
series generator.

Those same identifiers are still useful for debugging a specific incident, so they aren't thrown
away — they go into trace spans instead, per [Traces](../reference/traces.md), where an
identifier's cost is proportional to trace volume and retention (both under the collector
operator's control) rather than unbounded like a Prometheus label would be. Metrics answer "how
much/how often," bounded by design; traces answer "which specific delivery," scoped by design. The
same event produces both, deliberately at different levels of detail.

## Why merge-group and pull-request queue tracking are two separate statistics

`merge_group_events_total` and the per-pull-request `merge_queue_pr_outcomes_total` look like they
should be the same data viewed two ways, but they're computed from disjoint webhook deliveries and
never cross-reference each other. A merge group can contain multiple pull requests, and GitHub
doesn't deliver merge-group and pull-request webhooks in an order that provides a reliable join
key between "this merge group resolved" and "these N pull requests were in it." Rather than build
a probabilistic or best-effort correlation — which would be wrong in ways that are hard to detect
— the two statistics are kept honestly independent: the merge-group metric is the authoritative
answer to "did this merge attempt as a whole succeed," and the pull-request metric is the
authoritative answer to "how long was this specific PR's queue attempt, and how did it end,"
and neither claims to answer the other's question.

## Why workflow traces use an independent trace identity

A completed `workflow_job` webhook is processed like any other authenticated delivery — through
`github.webhook.authenticate` and `github.webhook.process` under one `http.request` trace — but
the historical trace it projects (`github.workflow.job` and its `github.workflow.step` children)
gets its own, unrelated trace ID rather than becoming a child of that request's live trace. The
reason is what the data represents: the request's live trace describes *this HTTP call handling a
webhook*, which takes milliseconds, while the workflow trace describes *a CI/CD run that already
happened*, whose steps may span minutes or hours and carry their own timestamps. Nesting a
minutes-long historical timeline inside a millisecond-long live request span would misrepresent
both — a trace viewer would show a request that appears to take as long as the workflow it merely
reported on. Keeping them as separate trace identities lets each be interpreted correctly on its
own terms: one root per completed job, timed by the job's own `started_at`/`completed_at`, findable
independently in a trace backend by its `github.workflow.job.id` rather than by which webhook
request happened to report it.

## Why workflow context is correlated durably

The completed `workflow_job` payload does not contain the authoritative Actions trigger event.
Inferring merge-queue execution from a `gh-readonly-queue/...` branch would be convenient but
heuristic. Instead, authenticated `workflow_run` deliveries contribute a bounded event and
sanitized branch projection keyed by repository, run ID, and run attempt. Persisting that small
projection allows later completed jobs to retain the correct context across webhook ordering,
reruns, and process restarts without retaining full payloads. Missing or ambiguous metadata is
omitted rather than guessed.

Job and step links are derived from the already validated repository and positive run, job, and
step identifiers. This avoids trusting payload-provided URLs while giving trace backends such as
Sentry a direct link to the Actions job or step-log anchor. These URLs remain span-only so they do
not create metric cardinality or disclose data through logs.

## Why the singleton model instead of horizontal scale

Covered from the storage angle in [Architecture](architecture.md#why-sqlite-and-why-a-singleton):
SQLite's single-writer model made a single replica the natural fit, and the chart, upgrade
procedures, and backup tooling all exist to make that one replica's lifecycle safe rather than to
work around it. The [PodDisruptionBudget](../how-to/upgrade-a-deployment.md#if-you-have-a-poddisruptionbudget)
is a deliberate example of this honesty: it permits voluntary disruption instead of promising
availability it can't deliver for a singleton.
