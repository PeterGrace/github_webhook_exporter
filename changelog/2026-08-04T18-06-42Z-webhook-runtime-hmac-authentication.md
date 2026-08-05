# Webhook runtime configuration and HMAC authentication

- Added validated webhook body-limit, delivery-retention, and prune-interval runtime settings with documented defaults and redacted failures.
- Added an enabled-only repository authentication lookup that makes unknown and disabled repositories indistinguishable while decrypting secrets only after a successful match.
- Added exact GitHub `sha256=` signature parsing and constant-time HMAC-SHA-256 verification over borrowed request bytes.
- Added GitHub-compatible fixtures, byte-sensitivity checks, malformed-input coverage, and scans that prevent authentication errors and debug output from exposing sensitive or attacker-controlled values.
