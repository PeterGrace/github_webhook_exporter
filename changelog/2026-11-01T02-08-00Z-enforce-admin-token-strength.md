# Enforce admin token minimum length and independence from the master key

Date: 2026-11-01T02:08:00Z

## What changed

- `AdminToken::new` now rejects tokens shorter than 32 bytes (`MIN_ADMIN_TOKEN_LENGTH`), not
  only empty ones. The floor counts UTF-8 bytes, matching the master key's 256 bits.
- `RuntimeConfig::from_lookup` rejects a `GHE_ADMIN_TOKEN` equal to the encoded
  `GHE_MASTER_KEY`. Both failures return `ConfigError::Invalid { variable: "GHE_ADMIN_TOKEN" }`,
  which names the variable and never the value.
- The admin token text is held in `Zeroizing` while it is checked, and moved into
  `AdminToken` with `mem::take`, so no unzeroized plaintext copy is left behind.
- Unit tests cover the 31/32-byte boundary, the byte (not character) count, short tokens in the
  config redaction table, and the reused-master-key case. Test fixtures shorter than 32 bytes
  were lengthened.
- Reference docs and the chart README describe the new rule.

## Why

Spec 01 requires `GHE_ADMIN_TOKEN` to be an independent, high-entropy credential. The claims
audit (CLM-0058) started the binary with a 1-character token and with the master key reused as
the token, and both served the admin API.

## Alternatives considered

Checking length only in `config.rs` was rejected: putting the floor in `AdminToken::new` makes a
weak token unrepresentable wherever the type is built. The independence check stays in config,
because only config sees both secrets.

Length cannot prove entropy; 32 repeated characters still pass. The documented generator
(`openssl rand -hex 32`) produces 64 random hex characters.

This resolves the CLM-0058 security finding tracked by issue #113.

Commit-pinned implementation, test and demonstration evidence (EVD-0294, EVD-0295, EVD-0296) is
in the claims ledger. CLM-0058 is marked supported, and the config-security (CLM-0002) and root
(CLM-0001) rollups are refreshed. Both stay refuted because of other leaves (CLM-0051, CLM-0073).
