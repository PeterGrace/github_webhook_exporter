# Issue 46 final integration and security fixes

- Guarded Helm render/package destinations with canonical path checks, generated-output ownership
  markers, sibling staging, atomic replacement, and value-free failures.
- Replaced lossy tar listing parsing with Python standard-library member validation and extraction;
  added whitespace/newline, traversal, absolute-path, link, and extraction-suppression regressions.
- Rendered source and extracted charts once each, required all ten manifest files to be byte-equal,
  and passed those exact directories to schema, policy, and credential validators.
- Reworked credential scanning to parse YAML/JSON structurally through pinned `yq` plus Python,
  detect sensitive assignment keys/case variants and minified Secret objects, and preserve explicit
  external-Secret reference allowances and non-disclosing diagnostics.
- Made the ServiceMonitor schema strict at the document root and added a top-level typo fixture.
- Added immutable built-in schema provenance, checksums, licensing, and an exact refresh procedure.
- Hardened CI tool installation against existing and symlink targets with verified regular archive
  members, fresh staging files, and no-clobber atomic installation.
- Required `just workflow-test` inside CI after pinned tool installation and expanded the exact
  structural workflow contract.
- Added focused policy fixtures for host PID/IPC, per-container non-root enforcement, and every
  init-container policy branch beyond privileged mode.
- Clarified that operators must run `just helm-render` before inspecting `dist/rendered/`.
- Added `just helm-security-test` for output, archive, source/archive comparison, installer symlink,
  and workflow-order regression coverage.
- Kept nullable override keys optional in `values.schema.json`, matching Helm 4.2.3's removal of null
  defaults before schema validation while retaining strict validation whenever values are supplied.
