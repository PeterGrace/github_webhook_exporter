# Runtime and configuration foundation

Date: 2026-08-03 17:07:10 -0400

## Added

- Typed environment configuration with validated defaults and redacted failures.
- Zeroizing storage for the database encryption root key and administrative token.
- A composable Axum application state, router, and TCP server boundary.
- Safe JSON conversion for internal HTTP errors.
- Structured stderr tracing controlled by `RUST_LOG`.
- `just fmt` and `just test` project validation recipes.

## Changed

- Replaced the development-only starter process with a Tokio/Axum runtime.
- Removed obsolete `ctrlc`, `lazy_static`, `dotenv`, and `console-subscriber` dependencies.
