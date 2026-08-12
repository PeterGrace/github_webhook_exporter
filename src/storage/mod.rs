mod delivery_store;
mod merge_queue_store;
mod repository_store;
mod workflow_run_store;

use std::{fs::OpenOptions, io, path::Path, time::Duration};

#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;

pub use delivery_store::{DeliveryClaim, DeliveryStore, DeliveryStoreError};
pub use merge_queue_store::{
    CompletionTransition, EnqueueTransition, MergeQueueStore, MergeQueueStoreError,
};
pub use repository_store::{RepositoryStore, RepositoryStoreError};
use sqlx::{
    migrate::MigrateError,
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    SqlitePool,
};
use thiserror::Error;
pub(crate) use workflow_run_store::{WorkflowRunStore, WorkflowRunStoreError};

static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!();

/// Opens a hardened SQLite connection pool and applies all embedded migrations.
///
/// New database files are created with mode `0600` on Unix. Every pooled connection enables
/// foreign keys, uses WAL journaling, and waits at most five seconds for a busy database.
///
/// # Errors
///
/// Returns [`StorageError::FilePreparation`] when a new database file cannot be securely created,
/// [`StorageError::Open`] when SQLite cannot open the pool, or [`StorageError::Migration`] when an
/// embedded migration fails.
pub async fn open_database(path: &Path) -> Result<SqlitePool, StorageError> {
    create_database_file_if_absent(path).map_err(StorageError::FilePreparation)?;

    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(Duration::from_secs(5));
    let pool = SqlitePoolOptions::new()
        .connect_with(options)
        .await
        .map_err(StorageError::Open)?;
    MIGRATOR.run(&pool).await.map_err(StorageError::Migration)?;

    Ok(pool)
}

/// Verifies that the database can serve a minimal query.
///
/// # Errors
///
/// Returns [`StorageError::Unavailable`] when SQLite is busy or locked, and
/// [`StorageError::Internal`] for other query failures.
pub async fn probe_database(pool: &SqlitePool) -> Result<(), StorageError> {
    sqlx::query("SELECT 1")
        .execute(pool)
        .await
        .map(|_| ())
        .map_err(StorageError::from_query_error)
}

/// A redacted database startup or probe failure.
#[derive(Debug, Error)]
pub enum StorageError {
    /// The database file could not be created securely.
    #[error("failed to prepare SQLite database file")]
    FilePreparation(#[source] io::Error),
    /// SQLite could not open the configured database.
    #[error("failed to open SQLite database")]
    Open(#[source] sqlx::Error),
    /// An embedded database migration failed.
    #[error("failed to migrate SQLite database")]
    Migration(#[source] MigrateError),
    /// SQLite is temporarily busy or locked.
    #[error("SQLite database is temporarily unavailable")]
    Unavailable,
    /// A database operation failed unexpectedly.
    #[error("internal SQLite persistence failure")]
    Internal(#[source] sqlx::Error),
}

impl StorageError {
    pub(crate) fn from_query_error(error: sqlx::Error) -> Self {
        if sqlite_is_busy_or_locked(&error) {
            Self::Unavailable
        } else {
            Self::Internal(error)
        }
    }
}

pub(crate) fn sqlite_is_busy_or_locked(error: &sqlx::Error) -> bool {
    error
        .as_database_error()
        .and_then(|database_error| database_error.code())
        .and_then(|code| code.parse::<i32>().ok())
        .is_some_and(|code| matches!(code & 0xff, 5 | 6))
}

fn create_database_file_if_absent(path: &Path) -> io::Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);

    match options.open(path) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}
