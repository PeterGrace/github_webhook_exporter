# PR #100 review response

## Summary

- named the claimant and the constraint in the `set_global_default` expect message, so a future
  collision reports why the lib test binary's global dispatcher was already taken
- reviewed and declined one advisory comment on the unreachable `new_span` sentinel: `owns_span`
  gates every forward through `Registry::span_data`, so an escaped sentinel id fails the lookup and
  no-ops rather than closing a real span

## Review disposition

| Comment | Disposition |
| --- | --- |
| `otlp_test.rs:694` -- unreachable `new_span` sentinel | skipped; reviewer confirmed no change needed |
| `otlp_test.rs:779` -- `expect` couples fixtures to the global-default invariant | applied |

The reviewer's premise was verified rather than taken on trust: `tests/webhook_api.rs` does call
`set_global_default`, but `tests/` compiles to separate integration binaries, so it cannot collide
with the lib test binary's default.

## Files changed

- `src/telemetry/otlp_test.rs`
- `changelog/2026-09-01T21-19-00Z-pr-100-review-response.md`

## Verification

- `just fmt`
- `cargo clippy --all-targets -- -D warnings`
- `just test`
