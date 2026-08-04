use std::{fs, time::Duration};

use github_webhook_exporter::{
    domain::repository::RepositoryMutation,
    security::{
        CanonicalRepositoryName, EncryptedRepositorySecret, MasterKey, RepositorySecret,
        RepositorySecretCipher, SecurityError,
    },
    storage::{open_database, probe_database, RepositoryStore, RepositoryStoreError},
};
use sqlx::{Row, SqlitePool};

fn name(value: &str) -> CanonicalRepositoryName {
    CanonicalRepositoryName::new(value).expect("test repository name is valid")
}

fn secret(value: &str) -> RepositorySecret {
    RepositorySecret::new(value.to_owned()).expect("test repository secret is valid")
}

async fn test_store(key_byte: u8) -> (tempfile::TempDir, SqlitePool, RepositoryStore) {
    let directory = tempfile::tempdir().expect("temporary directory is created");
    let pool = open_database(&directory.path().join("exporter.sqlite3"))
        .await
        .expect("database opens and migrates");
    let cipher = RepositorySecretCipher::new(
        &MasterKey::from_slice(&[key_byte; 32]).expect("test master key is valid"),
    )
    .expect("repository cipher is created");
    let store = RepositoryStore::new(pool.clone(), cipher);
    (directory, pool, store)
}

#[tokio::test]
async fn database_startup_migrates_and_hardens_every_connection() {
    let directory = tempfile::tempdir().expect("temporary directory is created");
    let database_path = directory.path().join("exporter.sqlite3");
    let pool = open_database(&database_path)
        .await
        .expect("database opens and migrates");

    let columns = sqlx::query("PRAGMA table_info(repositories)")
        .fetch_all(&pool)
        .await
        .expect("repository schema is inspectable")
        .into_iter()
        .map(|row| {
            (
                row.get::<String, _>("name"),
                row.get::<String, _>("type"),
                row.get::<i64, _>("notnull"),
                row.get::<i64, _>("pk"),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        columns,
        vec![
            ("id".to_owned(), "INTEGER".to_owned(), 0, 1),
            ("full_name".to_owned(), "TEXT".to_owned(), 1, 0),
            (
                "webhook_secret_ciphertext".to_owned(),
                "BLOB".to_owned(),
                1,
                0,
            ),
            ("webhook_secret_nonce".to_owned(), "BLOB".to_owned(), 1, 0,),
            ("encryption_version".to_owned(), "INTEGER".to_owned(), 1, 0),
            ("enabled".to_owned(), "INTEGER".to_owned(), 1, 0),
            ("created_at".to_owned(), "TEXT".to_owned(), 1, 0),
            ("updated_at".to_owned(), "TEXT".to_owned(), 1, 0),
        ]
    );

    let mut first_connection = pool.acquire().await.expect("first connection is available");
    let mut second_connection = pool
        .acquire()
        .await
        .expect("second connection is available");
    for connection in [&mut first_connection, &mut second_connection] {
        let foreign_keys: i64 = sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&mut **connection)
            .await
            .expect("foreign key setting is readable");
        let journal_mode: String = sqlx::query_scalar("PRAGMA journal_mode")
            .fetch_one(&mut **connection)
            .await
            .expect("journal setting is readable");
        let busy_timeout: i64 = sqlx::query_scalar("PRAGMA busy_timeout")
            .fetch_one(&mut **connection)
            .await
            .expect("busy timeout is readable");

        assert_eq!(foreign_keys, 1);
        assert_eq!(journal_mode, "wal");
        assert_eq!(busy_timeout, Duration::from_secs(5).as_millis() as i64);
    }
    probe_database(&pool)
        .await
        .expect("healthy database probe succeeds");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let mode = std::fs::metadata(&database_path)
            .expect("database metadata is readable")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }
}

#[tokio::test]
async fn repository_store_crud_returns_only_canonical_metadata() {
    let (_directory, _pool, store) = test_store(7).await;

    let created = store
        .create(name(" Owner/Repo "), secret("webhook-value"), true)
        .await
        .expect("repository is created");

    assert_eq!(created.full_name(), "owner/repo");
    assert!(created.enabled());
    assert!(!created.created_at().as_str().is_empty());
    assert_eq!(created.created_at(), created.updated_at());
    assert_eq!(
        store.get(created.id()).await.expect("repository exists"),
        created
    );
    assert_eq!(
        store.list().await.expect("repositories are listed"),
        vec![created.clone()]
    );

    let disabled = store
        .update(created.id(), RepositoryMutation::new().with_enabled(false))
        .await
        .expect("repository is disabled");
    assert!(!disabled.enabled());

    store
        .delete(created.id())
        .await
        .expect("repository is deleted");
    assert!(matches!(
        store.get(created.id()).await,
        Err(RepositoryStoreError::NotFound)
    ));
}

#[tokio::test]
async fn repository_store_maps_conflicts_and_missing_mutations() {
    let (_directory, _pool, store) = test_store(7).await;
    let existing = store
        .create(name("owner/repo"), secret("first-secret"), true)
        .await
        .expect("first repository is created");

    assert!(matches!(
        store
            .create(name("OWNER/REPO"), secret("second-secret"), false)
            .await,
        Err(RepositoryStoreError::Conflict)
    ));
    let missing_id =
        github_webhook_exporter::domain::repository::RepositoryId::new(existing.id().get() + 1)
            .expect("next positive identifier is valid");
    assert!(matches!(
        store
            .update(missing_id, RepositoryMutation::new().with_enabled(false),)
            .await,
        Err(RepositoryStoreError::NotFound)
    ));
    assert!(matches!(
        store.delete(missing_id).await,
        Err(RepositoryStoreError::NotFound)
    ));
    assert!(matches!(
        store.update(existing.id(), RepositoryMutation::new()).await,
        Err(RepositoryStoreError::EmptyMutation)
    ));
}

#[tokio::test]
async fn encrypted_rotation_and_rename_preserve_associated_data_invariants() {
    const PLAINTEXT_MARKER: &str = "plaintext-webhook-secret-marker";
    let (directory, pool, store) = test_store(7).await;
    let created = store
        .create(name("owner/repo"), secret(PLAINTEXT_MARKER), true)
        .await
        .expect("repository is created");
    let original = encrypted_fields(&pool, created.id().get()).await;

    store
        .update(
            created.id(),
            RepositoryMutation::new().with_webhook_secret(secret("rotated-secret")),
        )
        .await
        .expect("secret rotates");
    let rotated = encrypted_fields(&pool, created.id().get()).await;
    assert_ne!(original.0, rotated.0);
    assert_ne!(original.1, rotated.1);

    let renamed = store
        .update(
            created.id(),
            RepositoryMutation::new().with_full_name(name("new-owner/new-repo")),
        )
        .await
        .expect("repository renames");
    assert_eq!(renamed.full_name(), "new-owner/new-repo");
    let renamed_fields = encrypted_fields(&pool, created.id().get()).await;
    let encrypted = EncryptedRepositorySecret::from_parts(
        u8::try_from(renamed_fields.2).expect("version fits in u8"),
        &renamed_fields.1,
        renamed_fields.0,
    )
    .expect("stored encrypted fields are valid");
    let cipher = RepositorySecretCipher::new(
        &MasterKey::from_slice(&[7_u8; 32]).expect("test key is valid"),
    )
    .expect("cipher is created");
    assert_eq!(
        cipher
            .decrypt(&name("new-owner/new-repo"), &encrypted)
            .expect("new associated data decrypts")
            .expose_secret(),
        "rotated-secret"
    );
    assert_eq!(
        cipher
            .decrypt(&name("owner/repo"), &encrypted)
            .expect_err("old associated data must fail"),
        SecurityError::DecryptionFailed
    );

    let selected = encrypted_fields(&pool, created.id().get()).await;
    for stored_bytes in [&selected.0, &selected.1] {
        for plaintext in [PLAINTEXT_MARKER, "rotated-secret"] {
            assert!(!stored_bytes
                .windows(plaintext.len())
                .any(|window| window == plaintext.as_bytes()));
        }
    }

    drop(store);
    pool.close().await;
    for entry in fs::read_dir(directory.path()).expect("database directory is readable") {
        let bytes = fs::read(entry.expect("directory entry is readable").path())
            .expect("database artifact is readable");
        for plaintext in [PLAINTEXT_MARKER, "rotated-secret"] {
            assert!(!bytes
                .windows(plaintext.len())
                .any(|window| window == plaintext.as_bytes()));
        }
    }
}

#[tokio::test]
async fn encrypted_conflicts_wrong_keys_and_tampering_fail_closed_atomically() {
    let (_directory, pool, store) = test_store(7).await;
    let first = store
        .create(name("owner/first"), secret("first-secret"), true)
        .await
        .expect("first repository is created");
    store
        .create(name("owner/second"), secret("second-secret"), true)
        .await
        .expect("second repository is created");
    let before = encrypted_fields(&pool, first.id().get()).await;

    assert!(matches!(
        store
            .update(
                first.id(),
                RepositoryMutation::new().with_full_name(name("owner/second")),
            )
            .await,
        Err(RepositoryStoreError::Conflict)
    ));
    assert_eq!(encrypted_fields(&pool, first.id().get()).await, before);
    assert_eq!(
        store
            .get(first.id())
            .await
            .expect("failed rename leaves original readable")
            .full_name(),
        "owner/first"
    );

    let wrong_key_store = RepositoryStore::new(
        pool.clone(),
        RepositorySecretCipher::new(
            &MasterKey::from_slice(&[8_u8; 32]).expect("wrong test key is valid"),
        )
        .expect("wrong-key cipher is constructed"),
    );
    assert!(matches!(
        wrong_key_store.get(first.id()).await,
        Err(RepositoryStoreError::Cryptographic(_))
    ));

    sqlx::query(
        "UPDATE repositories SET webhook_secret_ciphertext = \
         zeroblob(length(webhook_secret_ciphertext)) WHERE id = ?",
    )
    .bind(first.id().get())
    .execute(&pool)
    .await
    .expect("test row is tampered");
    assert!(matches!(
        store.get(first.id()).await,
        Err(RepositoryStoreError::Cryptographic(_))
    ));
    assert!(matches!(
        store.list().await,
        Err(RepositoryStoreError::Cryptographic(_))
    ));
}

async fn encrypted_fields(pool: &SqlitePool, id: i64) -> (Vec<u8>, Vec<u8>, i64) {
    let row = sqlx::query(
        "SELECT webhook_secret_ciphertext, webhook_secret_nonce, encryption_version \
         FROM repositories WHERE id = ?",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .expect("encrypted fields are readable");
    (
        row.get("webhook_secret_ciphertext"),
        row.get("webhook_secret_nonce"),
        row.get("encryption_version"),
    )
}
