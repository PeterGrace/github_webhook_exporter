CREATE TABLE repositories (
    id INTEGER PRIMARY KEY,
    full_name TEXT NOT NULL UNIQUE,
    webhook_secret_ciphertext BLOB NOT NULL,
    webhook_secret_nonce BLOB NOT NULL,
    encryption_version INTEGER NOT NULL,
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL
);
