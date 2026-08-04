use crate::{
    domain::repository::{RepositoryId, RepositoryMetadata, RepositoryMutation},
    security::{
        CanonicalRepositoryName, EncryptedRepositorySecret, RepositorySecret,
        RepositorySecretCipher, SecurityError,
    },
};
use sqlx::{sqlite::SqliteRow, Row, SqlitePool};
use thiserror::Error;

use super::sqlite_is_busy_or_locked;

const RETURNING_COLUMNS: &str = "id, full_name, webhook_secret_ciphertext, \
    webhook_secret_nonce, encryption_version, enabled, created_at, updated_at";

/// Transactional persistence for encrypted repository configurations.
pub struct RepositoryStore {
    pool: SqlitePool,
    cipher: RepositorySecretCipher,
}

impl RepositoryStore {
    /// Creates a store from a migrated pool and repository-secret cipher.
    pub fn new(pool: SqlitePool, cipher: RepositorySecretCipher) -> Self {
        Self { pool, cipher }
    }

    /// Encrypts and inserts a repository configuration in one transaction.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryStoreError::Conflict`] for an existing canonical name,
    /// [`RepositoryStoreError::Cryptographic`] when encryption fails, and a redacted persistence
    /// error for SQLite failures.
    pub async fn create(
        &self,
        full_name: CanonicalRepositoryName,
        webhook_secret: RepositorySecret,
        enabled: bool,
    ) -> Result<RepositoryMetadata, RepositoryStoreError> {
        let mut transaction = self.pool.begin().await.map_err(map_sqlx_error)?;
        let encrypted = self
            .cipher
            .encrypt(&full_name, &webhook_secret)
            .map_err(RepositoryStoreError::Cryptographic)?;
        let query = format!(
            "INSERT INTO repositories (full_name, webhook_secret_ciphertext, \
             webhook_secret_nonce, encryption_version, enabled, created_at, updated_at) \
             VALUES (?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
             strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) RETURNING {RETURNING_COLUMNS}"
        );
        let row = sqlx::query(&query)
            .bind(full_name.as_str())
            .bind(encrypted.ciphertext())
            .bind(encrypted.nonce().as_slice())
            .bind(i64::from(encrypted.version()))
            .bind(enabled)
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx_error)?;
        let metadata = self.authenticate_row(row)?;
        transaction.commit().await.map_err(map_sqlx_error)?;
        Ok(metadata)
    }

    /// Lists all repository metadata in identifier order after authenticating every stored secret.
    ///
    /// # Errors
    ///
    /// Returns a cryptographic failure if any persisted encrypted value is invalid, and a redacted
    /// persistence error when SQLite cannot complete the query.
    pub async fn list(&self) -> Result<Vec<RepositoryMetadata>, RepositoryStoreError> {
        let query = format!("SELECT {RETURNING_COLUMNS} FROM repositories ORDER BY id");
        sqlx::query(&query)
            .fetch_all(&self.pool)
            .await
            .map_err(map_sqlx_error)?
            .into_iter()
            .map(|row| self.authenticate_row(row))
            .collect()
    }

    /// Fetches repository metadata by identifier after authenticating its stored secret.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryStoreError::NotFound`] for an unknown identifier, a cryptographic
    /// failure for invalid encrypted storage, or a redacted persistence failure.
    pub async fn get(&self, id: RepositoryId) -> Result<RepositoryMetadata, RepositoryStoreError> {
        let query = format!("SELECT {RETURNING_COLUMNS} FROM repositories WHERE id = ?");
        let row = sqlx::query(&query)
            .bind(id.get())
            .fetch_optional(&self.pool)
            .await
            .map_err(map_sqlx_error)?
            .ok_or(RepositoryStoreError::NotFound)?;
        self.authenticate_row(row)
    }

    /// Applies a non-empty repository mutation in one transaction.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryStoreError::EmptyMutation`] when no field is supplied,
    /// [`RepositoryStoreError::NotFound`] for an unknown identifier, and a typed cryptographic or
    /// persistence error when validation or SQLite fails.
    pub async fn update(
        &self,
        id: RepositoryId,
        mutation: RepositoryMutation,
    ) -> Result<RepositoryMetadata, RepositoryStoreError> {
        if mutation.is_empty() {
            return Err(RepositoryStoreError::EmptyMutation);
        }

        let mut transaction = self.pool.begin().await.map_err(map_sqlx_error)?;
        let select = format!("SELECT {RETURNING_COLUMNS} FROM repositories WHERE id = ?");
        let current = sqlx::query(&select)
            .bind(id.get())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx_error)?
            .ok_or(RepositoryStoreError::NotFound)
            .and_then(decode_row)?;
        // Authenticate before every mutation, even when a replacement secret was supplied. This
        // prevents a write from silently legitimizing a tampered row.
        let current_secret = self
            .cipher
            .decrypt(&current.full_name, &current.encrypted)
            .map_err(RepositoryStoreError::Cryptographic)?;
        let final_name = mutation
            .full_name
            .unwrap_or_else(|| current.full_name.clone());
        let enabled = mutation.enabled.unwrap_or(current.enabled);
        let encrypted = if final_name != current.full_name || mutation.webhook_secret.is_some() {
            self.cipher
                .encrypt(
                    &final_name,
                    mutation.webhook_secret.as_ref().unwrap_or(&current_secret),
                )
                .map_err(RepositoryStoreError::Cryptographic)?
        } else {
            current.encrypted
        };
        let update = format!(
            "UPDATE repositories SET full_name = ?, webhook_secret_ciphertext = ?, \
             webhook_secret_nonce = ?, encryption_version = ?, enabled = ?, \
             updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
             WHERE id = ? RETURNING {RETURNING_COLUMNS}"
        );
        let row = sqlx::query(&update)
            .bind(final_name.as_str())
            .bind(encrypted.ciphertext())
            .bind(encrypted.nonce().as_slice())
            .bind(i64::from(encrypted.version()))
            .bind(enabled)
            .bind(id.get())
            .fetch_one(&mut *transaction)
            .await
            .map_err(map_sqlx_error)?;
        let metadata = self.authenticate_row(row)?;
        transaction.commit().await.map_err(map_sqlx_error)?;
        Ok(metadata)
    }

    /// Deletes a repository configuration in one transaction.
    ///
    /// # Errors
    ///
    /// Returns [`RepositoryStoreError::NotFound`] for an unknown identifier, refuses to delete a
    /// row whose encrypted value cannot be authenticated, and maps SQLite failures to redacted
    /// persistence errors.
    pub async fn delete(&self, id: RepositoryId) -> Result<(), RepositoryStoreError> {
        let mut transaction = self.pool.begin().await.map_err(map_sqlx_error)?;
        let select = format!("SELECT {RETURNING_COLUMNS} FROM repositories WHERE id = ?");
        let row = sqlx::query(&select)
            .bind(id.get())
            .fetch_optional(&mut *transaction)
            .await
            .map_err(map_sqlx_error)?
            .ok_or(RepositoryStoreError::NotFound)?;
        self.authenticate_row(row)?;
        sqlx::query("DELETE FROM repositories WHERE id = ?")
            .bind(id.get())
            .execute(&mut *transaction)
            .await
            .map_err(map_sqlx_error)?;
        transaction.commit().await.map_err(map_sqlx_error)?;
        Ok(())
    }

    fn authenticate_row(&self, row: SqliteRow) -> Result<RepositoryMetadata, RepositoryStoreError> {
        let stored = decode_row(row)?;
        self.cipher
            .decrypt(&stored.full_name, &stored.encrypted)
            .map_err(RepositoryStoreError::Cryptographic)?;
        Ok(stored.into_metadata())
    }
}

struct StoredRepository {
    id: RepositoryId,
    full_name: CanonicalRepositoryName,
    encrypted: EncryptedRepositorySecret,
    enabled: bool,
    created_at: String,
    updated_at: String,
}

impl StoredRepository {
    fn into_metadata(self) -> RepositoryMetadata {
        RepositoryMetadata::from_database(
            self.id,
            self.full_name,
            self.enabled,
            self.created_at,
            self.updated_at,
        )
    }
}

fn decode_row(row: SqliteRow) -> Result<StoredRepository, RepositoryStoreError> {
    let id = RepositoryId::from_database(row.try_get("id").map_err(map_sqlx_error)?)
        .ok_or(RepositoryStoreError::InternalData)?;
    let full_name = CanonicalRepositoryName::new(
        row.try_get::<String, _>("full_name")
            .map_err(map_sqlx_error)?
            .as_str(),
    )
    .map_err(RepositoryStoreError::Cryptographic)?;
    let version = u8::try_from(
        row.try_get::<i64, _>("encryption_version")
            .map_err(map_sqlx_error)?,
    )
    .map_err(|_| RepositoryStoreError::Cryptographic(SecurityError::InvalidEncryptedSecret))?;
    let encrypted = EncryptedRepositorySecret::from_parts(
        version,
        &row.try_get::<Vec<u8>, _>("webhook_secret_nonce")
            .map_err(map_sqlx_error)?,
        row.try_get("webhook_secret_ciphertext")
            .map_err(map_sqlx_error)?,
    )
    .map_err(RepositoryStoreError::Cryptographic)?;
    let enabled = match row.try_get::<i64, _>("enabled").map_err(map_sqlx_error)? {
        0 => false,
        1 => true,
        _ => return Err(RepositoryStoreError::InternalData),
    };

    Ok(StoredRepository {
        id,
        full_name,
        encrypted,
        enabled,
        created_at: row.try_get("created_at").map_err(map_sqlx_error)?,
        updated_at: row.try_get("updated_at").map_err(map_sqlx_error)?,
    })
}

/// A stable, redacted repository persistence failure.
#[derive(Debug, Error)]
pub enum RepositoryStoreError {
    /// No repository exists with the requested identifier.
    #[error("repository was not found")]
    NotFound,
    /// The canonical repository name already exists.
    #[error("repository name already exists")]
    Conflict,
    /// A repository update did not contain any fields.
    #[error("repository mutation must not be empty")]
    EmptyMutation,
    /// Stored encryption or a cryptographic operation failed closed.
    #[error("repository cryptographic operation failed")]
    Cryptographic(#[source] SecurityError),
    /// SQLite is temporarily busy or locked.
    #[error("repository storage is temporarily unavailable")]
    Unavailable,
    /// Persisted values violated an internal invariant.
    #[error("internal repository persistence data failure")]
    InternalData,
    /// SQLite returned an unexpected persistence failure.
    #[error("internal repository persistence failure")]
    Internal(#[source] sqlx::Error),
}

fn map_sqlx_error(error: sqlx::Error) -> RepositoryStoreError {
    if error
        .as_database_error()
        .is_some_and(sqlx::error::DatabaseError::is_unique_violation)
    {
        RepositoryStoreError::Conflict
    } else if sqlite_is_busy_or_locked(&error) {
        RepositoryStoreError::Unavailable
    } else {
        RepositoryStoreError::Internal(error)
    }
}
