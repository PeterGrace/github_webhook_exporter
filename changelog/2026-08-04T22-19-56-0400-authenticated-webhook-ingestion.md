# Authenticated GitHub webhook ingestion

- Added public `POST /webhooks/github` routing with exact JSON content-type validation, required
  GitHub header validation, configured body limiting, minimal JSON projection, and stable redacted
  `400`, `401`, `413`, `415`, and `503` responses.
- Composed existing enabled-repository HMAC authentication and atomic delivery claims so no claim,
  event normalization, or event/body metric update occurs before authentication.
- Recorded request results and durations for every webhook response, event/body metrics only for
  authenticated new claims, and duplicate metrics without double-counting events.
- Added bounded structured outcome/failure logging with opaque error correlation IDs and no
  attacker-controlled or secret fields.
- Added complete-router tests for authentication, byte sensitivity, malformed inputs, body limits,
  indistinguishable unauthorized responses, duplicates, database failures, metrics, persistence,
  and sensitive-output redaction.
- Documented the endpoint contract and operational behavior in `docs/operations.md`.
