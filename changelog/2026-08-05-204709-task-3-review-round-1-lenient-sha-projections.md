# Task 3 Review Round 1: Lenient SHA Projections

## Summary

- Preserved authenticated webhook acceptance when pull-request or merge-group SHA fields contain non-string JSON values.
- Changed the optional SHA projections to retain untrusted JSON only until `Value::as_str` can safely extract a candidate.
- Continued validating string candidates through `CommitSha`, so malformed and non-string values remain absent from span attributes.
- Added OTLP and webhook API regressions for numeric pull-request SHA and object merge-group SHA values.

## Compatibility

These SHA fields were previously ignored by webhook processing. Adding telemetry must not make their JSON types part of the accepted webhook contract, so unsupported values are omitted rather than causing a `400 Bad Request` response.
