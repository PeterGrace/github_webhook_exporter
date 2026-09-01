# otlp_test: intermittent missing-span failures from a leaked parent span reference

## Summary

- found and fixed the root cause of the intermittent `telemetry::otlp_test` failures reported in
  issue #99, where a span that should have been exported was absent from the capture
- routed ambient span closures back to the active fixture with a process-wide default dispatcher, so
  a child span released on a library-owned thread still closes its parent
- replaced the silent give-up in `flush_after_span_closure` with an assertion that names the spans
  still open, so an incomplete capture can never again present itself as an unexplained span list
- added a deterministic regression test that reproduces the leak without needing CPU contention

## Root cause

`tracing-subscriber`'s registry keeps a parent span open on behalf of each of its children, and
releases that reference from `DataInner::clear`:

```rust
let subscriber = dispatcher::get_default(Dispatch::clone);
if let Some(parent) = self.parent.take() {
    let _ = subscriber.try_close(parent);
}
```

The release resolves its subscriber through `tracing::dispatcher::get_default` -- the *ambient*
dispatcher of whichever thread runs the clear -- rather than through the dispatcher the span was
created with. `sharded-slab` defers that clear to the thread which drops the last reference to the
span's slot, which under CPU contention is sometimes a library-owned thread with no dispatcher of
its own.

Production is unaffected: `telemetry::init` installs a process-wide default, so the release always
reaches the real subscriber. The fixtures instead attached their subscriber per-future with
`WithSubscriber` and installed no global default, so on such a thread `get_default` returned
`NoSubscriber`, the parent's reference count leaked permanently, and the parent span never closed.
`tracing-opentelemetry` ends a span from its `on_close` callback, so a span that never closes is
never exported -- which is why waiting longer never helped.

Instrumenting `DataInner::clear` in a vendored `tracing-subscriber` confirmed this directly. Every
failing run contained at least one clear whose ambient dispatcher was `NoSubscriber`, on a
`sqlx-sqlite-worker` thread; every passing run contained none:

```
DIAG CLEAR name=sqlite.query ambient_noop=true thread=sqlx-sqlite-worker-72
```

## Fix

`FixtureSpanCloser` is installed once as the test binary's global default dispatcher. It behaves
exactly like `NoSubscriber` -- it reports every callsite disabled and records nothing -- except for
`try_close`, which it forwards to the fixture currently registered. Forwarding is guarded by an
ownership check, because a span identifier is only meaningful to the registry that minted it.

`register_fixture_dispatch` takes the held telemetry test lock and returns it inside the
registration guard. Bundling the two makes the ordering structural: the registration is cleared
before the lock is released, so one fixture can never clear its successor's registration. An earlier
revision kept the two guards separate and hit exactly that bug.

## Files changed

- `src/telemetry/otlp_test.rs`
- `changelog/2026-09-01T20-45-08Z-otlp-test-parent-span-close-leak.md`

## Verification

- `just fmt`
- `cargo clippy --all-targets -- -D warnings`
- `just test` -- 286 lib tests and every integration target pass
- `cargo test --lib parent_spans_close_when_a_child_is_released_off_a_dispatcher_thread` -- fails
  without the routing dispatcher, passes with it
- 30 consecutive full lib-suite runs with half the cores saturated: 0 failures. The same harness
  reproduced the flake at 2/10 and 2/12 before the fix.
- With the clear site instrumented, each suite run performs 7-16 parent releases on threads that
  carry no dispatcher of their own. All of them are now forwarded to the owning registry; before the
  fix every one was silently discarded.
