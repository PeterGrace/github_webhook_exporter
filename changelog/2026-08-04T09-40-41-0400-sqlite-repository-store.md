# SQLite migrations and encrypted repository store

Date: 2026-08-04 09:40:41 -0400

## Changed

- Add an embedded migration for the repository configuration schema.
- Open SQLite through a SQLx pool with WAL journaling, foreign keys, and a five-second busy timeout on every connection.
- Create new Unix database files with owner-only `0600` permissions.
- Add typed repository identifiers, timestamps, metadata, and mutation values.
- Add transactional create, list, fetch, update, rename, secret rotation, and delete operations that expose metadata only.
- Authenticate encrypted rows on reads and mutations, and re-encrypt secrets against renamed canonical repository names in the same transaction.
- Map missing rows, canonical-name conflicts, cryptographic failures, busy databases, migrations, and internal persistence failures to redacted error categories.

## Validation

- Exercise schema migration, connection pragmas, database probing, and Unix file permissions against temporary SQLite databases.
- Cover CRUD, uniqueness, missing identifiers, empty mutations, nonce and ciphertext rotation, rename associated data, conflict rollback, wrong keys, tampering, and plaintext absence from selected fields and database artifacts.
