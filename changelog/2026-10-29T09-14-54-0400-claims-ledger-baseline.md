# Claims ledger baseline

Date: 2026-10-29T13:14:54Z

## What changed

- Added `claims.json`, a claims-mcp ledger auditing the project against its specs
  (`docs/superpowers/specs/`), `docs/build-spec.md`, the user-facing docs and `.claude/CLAUDE.md`.
- Ignored `.claims.json.lock`, the claims-mcp server's lock file.

No source code changed.

## Ledger shape

- CLM-0001: root claim (the design decomposition and README summary).
- CLM-0002..CLM-0007: area parents, tagged `area:config-security`, `area:ingestion-metrics`,
  `area:merge-queue`, `area:otlp`, `area:packaging`, `area:docs-constraints`.
- CLM-0008..CLM-0208: 201 leaf claims with evidence and FEVER verdicts.

Six claude-sonnet-5-5 auditors (one per area) gathered evidence from code reading and targeted
`cargo test`, `helm lint`/`template`, `just` self-test recipes and a local run of the binary. The
coordinator (claude-opus-5-5) spot-checked the top findings and recorded the parent roll-ups. No
Docker or kind cluster was used. Integrity seals verify.

## Result

181 supported, 24 refuted (17 leaves plus all 7 parents), 3 not enough info.

Refuted leaves, highest integrity level first:

| Claim | IL | Finding |
|---|---|---|
| CLM-0195 | 4 | OTLP exporter HTTP client (`src/telemetry.rs` `build_blocking_http_client`) sets no redirect policy; custom `OTEL_EXPORTER_OTLP_HEADERS` can be forwarded cross-host |
| CLM-0037 | 3 | Webhook branch names with `..` segments are not rejected; the GitHub API URL carrying the installation token can normalize to another path |
| CLM-0058 | 3 | `GHE_ADMIN_TOKEN` is only checked for emptiness; a 1-character token or one equal to the master key is accepted |
| CLM-0097 | 3 | `cargo clippy --all-targets -- -D warnings` fails on clippy 0.1.99 (`double_must_use` at `src/github/refresh.rs:80`, `src/retention.rs:265`) |
| CLM-0121 | 3 | `maintenanceMode=true` render fails Conftest GWE001; no CI render case covers it |
| CLM-0132 | 3 | Release publish/version tests and Helm security/lifecycle unit tests are not run in CI |
| CLM-0168 | 2 | Two trace-rejection `warn!` calls log repository names and delivery IDs, against spec 02 |
| CLM-0194 | 2 | Spec 04 forbids repo/PR/SHA/delivery span attributes that later specs and the code emit |
| CLM-0081 | 2 | README says SQLite holds three kinds of data; there are six tables |
| CLM-0038, CLM-0051, CLM-0073, CLM-0082, CLM-0083, CLM-0100, CLM-0169, CLM-0170 | 1 | Documentation drift |

Not enough info: CLM-0031 (GitHub client timeout and body cap untested; body is buffered before
the cap), CLM-0106 and CLM-0107 (container smoke and reproducibility need a Docker host).

Remaining gap: CLM-0087 (`.env` gitignored, IL3) is checked by inspection only (`rigor`).

## Working with the ledger

The claims MCP server writes `claims.json` in the working directory of the session that started
it. Never edit the file by hand; every record is sealed.
