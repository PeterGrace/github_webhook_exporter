# Authenticated repository configuration API

- Added bearer-authenticated Axum routes for creating, listing, fetching, atomically updating, and deleting repository webhook configuration.
- Added strict repository-name, secret-length, JSON, PATCH, and identifier validation with stable JSON error envelopes.
- Kept every API response metadata-only and added full-router tests for credential handling, CRUD behavior, conflicts, transaction rollback, secret rotation, and response/log redaction.
- Initialized the migrated SQLite repository store and administrator authenticator before accepting HTTP traffic.
