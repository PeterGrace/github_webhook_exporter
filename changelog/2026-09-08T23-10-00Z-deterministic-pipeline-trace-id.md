# Deterministic pipeline-run trace identifiers

**Date:** 2026-09-08
**Issue:** #106

## What changed

The pipeline-run summary trace's root span no longer takes a random trace ID from the SDK's
generator. It is now derived from the run itself:

```text
trace_id = sha256("gha-pipeline:{repository}:{run_id}:{run_attempt}")[0..16]
```

`{repository}` is the canonical lowercase `owner/repository` name; the two identifiers are decimal.
Nothing else changed: job traces, span structure, span links, and every attribute are untouched.

## Why

A consumer emitting OTLP records from *inside* a GitHub Actions job wants those records to appear
on the run's trace in Sentry, which associates a log with a trace via the record's `traceId`. It
cannot learn the exporter's trace ID: the pipeline spans are minted from the *completed*
`workflow_run` webhook, strictly after the in-job step runs, so at emit time the ID does not exist
yet. Looking it up is impossible in principle, not merely awkward.

The only remaining option is for both sides to compute the same value independently from data
GitHub gives to both. `$GITHUB_REPOSITORY`, `$GITHUB_RUN_ID`, and `$GITHUB_RUN_ATTEMPT` are free
environment variables inside every job, and the exporter already reads all three at the emit site.

The pipeline root is the right anchor rather than the job traces: it is already one per run
attempt, already a root built from an empty parent context, and holds roughly one span per job
rather than one per step, so anchoring here does not concentrate every step span into one trace.

## Implementation notes

`opentelemetry` 0.32's `SpanBuilder` exposes no trace-identifier field, and `IdGenerator::new_trace_id`
receives no per-span context, so a custom generator cannot see which run it is minting for. The
only remaining lever is the parent context passed to `build_with_context`, which the SDK reads the
trace ID from. Two details of that synthetic parent are load bearing and were confirmed against the
installed SDK source rather than assumed:

- Its span ID must be `SpanId::INVALID`. `SdkTracer::build_recording_span` copies the parent's span
  ID straight into `parent_span_id`, so any real value would leave the pipeline root looking like
  the child of a span that was never exported. An invalid one keeps `parent_span_id` zero and
  `parent_span_is_remote` false — byte-for-byte the shape an empty context produced before.
- Its trace flags must be `TraceFlags::SAMPLED`. The default `ParentBased(AlwaysOn)` sampler
  branches on `has_active_span()`, which is `self.span.is_some()` and does not check validity, so
  the synthetic parent *is* consulted. An unsampled one would drop the entire pipeline trace.

The repository string is the canonical lowercased form, because that is what the exporter holds:
`CanonicalRepositoryName` trims and lowercases on construction, and the raw payload name is not
retained. Consumers must therefore lowercase `$GITHUB_REPOSITORY` before hashing. The contract and
a reproducible shell recipe are pinned in `book/src/reference/traces.md` with two fixture vectors
that the unit tests assert, so neither side can drift silently.

`sha2` and the `TraceId::from_bytes` pattern were already in the codebase; no dependency was added.

## Behaviour worth being deliberate about

- **Re-delivered webhooks.** A replayed `workflow_run` `completed` would now reuse the trace ID
  rather than mint a second pipeline trace. In practice the durable delivery claim already stops
  the second emission, so this is a belt-and-braces improvement rather than a change in what ships.
- **Partial re-runs.** `run_attempt` is in the key, so a re-run gets a distinct trace and jobs that
  did not re-execute are absent from it. This matches GitHub's own per-attempt reporting.
- **Retroactivity.** Traces exported before this change keep random IDs and cannot be joined.
- **All-zero digest.** `TraceId::INVALID` is rejected and the root falls back to the random
  generator, losing in-job association for that single run rather than emitting an invalid trace.

## Validation

- `just fmt`
- `cargo clippy --all-targets -- -D warnings`
- `just test`
- The documented shell recipe was executed and reproduces the `owner/repository:31:2` fixture.
