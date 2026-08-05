use github_webhook_exporter::{
    domain::delivery::DeliveryId,
    storage::{open_database, DeliveryClaim, DeliveryStore, DeliveryStoreError},
};
use sqlx::{Row, SqlitePool};

const DELIVERY_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

fn delivery_id(value: &str) -> DeliveryId {
    DeliveryId::parse(value).expect("test delivery identifier is valid")
}

async fn test_store() -> (tempfile::TempDir, SqlitePool, DeliveryStore) {
    let directory = tempfile::tempdir().expect("temporary directory is created");
    let pool = open_database(&directory.path().join("exporter.sqlite3"))
        .await
        .expect("database opens and migrates");
    let store = DeliveryStore::new(pool.clone());
    (directory, pool, store)
}

#[tokio::test]
async fn delivery_migration_has_only_the_bounded_claim_schema() {
    let (_directory, pool, _store) = test_store().await;

    let columns = sqlx::query("PRAGMA table_info(processed_deliveries)")
        .fetch_all(&pool)
        .await
        .expect("delivery schema is inspectable")
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
            ("delivery_id".to_owned(), "TEXT".to_owned(), 0, 1),
            ("received_at".to_owned(), "TEXT".to_owned(), 1, 0),
        ]
    );

    let indexes = sqlx::query("PRAGMA index_list(processed_deliveries)")
        .fetch_all(&pool)
        .await
        .expect("delivery indexes are inspectable");
    assert!(indexes
        .iter()
        .any(|row| { row.get::<String, _>("name") == "processed_deliveries_received_at_idx" }));
    let indexed_columns = sqlx::query("PRAGMA index_info(processed_deliveries_received_at_idx)")
        .fetch_all(&pool)
        .await
        .expect("retention index is inspectable")
        .into_iter()
        .map(|row| row.get::<String, _>("name"))
        .collect::<Vec<_>>();
    assert_eq!(indexed_columns, vec!["received_at"]);
}

#[tokio::test]
async fn duplicate_claim_preserves_the_original_receipt_time() {
    let (_directory, pool, store) = test_store().await;
    let id = delivery_id(DELIVERY_ID);

    assert_eq!(
        store.claim(&id).await.expect("first claim succeeds"),
        DeliveryClaim::New
    );
    let original_received_at: String =
        sqlx::query_scalar("SELECT received_at FROM processed_deliveries WHERE delivery_id = ?")
            .bind(DELIVERY_ID)
            .fetch_one(&pool)
            .await
            .expect("claimed delivery is readable");
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;

    assert_eq!(
        store.claim(&id).await.expect("duplicate claim succeeds"),
        DeliveryClaim::Duplicate
    );
    let retained_received_at: String =
        sqlx::query_scalar("SELECT received_at FROM processed_deliveries WHERE delivery_id = ?")
            .bind(DELIVERY_ID)
            .fetch_one(&pool)
            .await
            .expect("duplicate delivery remains readable");
    assert_eq!(retained_received_at, original_received_at);
}

#[tokio::test]
async fn delivery_claim_survives_database_reopen() {
    let directory = tempfile::tempdir().expect("temporary directory is created");
    let database_path = directory.path().join("exporter.sqlite3");
    let pool = open_database(&database_path)
        .await
        .expect("database opens and migrates");
    let store = DeliveryStore::new(pool.clone());
    let id = delivery_id(DELIVERY_ID);

    assert_eq!(
        store.claim(&id).await.expect("first claim succeeds"),
        DeliveryClaim::New
    );
    drop(store);
    pool.close().await;

    let reopened_pool = open_database(&database_path)
        .await
        .expect("database reopens");
    let reopened_store = DeliveryStore::new(reopened_pool);
    assert_eq!(
        reopened_store
            .claim(&id)
            .await
            .expect("persisted claim is detected"),
        DeliveryClaim::Duplicate
    );
}

#[tokio::test]
async fn concurrent_claims_produce_exactly_one_new_result() {
    let (_directory, _pool, store) = test_store().await;
    let id = delivery_id(DELIVERY_ID);
    let mut claims = tokio::task::JoinSet::new();

    for _ in 0..16 {
        let concurrent_store = store.clone();
        claims.spawn(async move { concurrent_store.claim(&id).await });
    }

    let mut new_count = 0;
    let mut duplicate_count = 0;
    while let Some(result) = claims.join_next().await {
        match result
            .expect("claim task completes")
            .expect("concurrent claim succeeds")
        {
            DeliveryClaim::New => new_count += 1,
            DeliveryClaim::Duplicate => duplicate_count += 1,
        }
    }
    assert_eq!(new_count, 1);
    assert_eq!(duplicate_count, 15);
}

#[tokio::test]
async fn delivery_store_maps_locked_database_to_unavailable() {
    let (_directory, pool, store) = test_store().await;
    let mut locking_connection = pool
        .acquire()
        .await
        .expect("locking connection is available");
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *locking_connection)
        .await
        .expect("write lock is acquired");
    let cutoff = time::OffsetDateTime::from_unix_timestamp(1_769_904_000)
        .expect("test cutoff timestamp is representable");
    let id = delivery_id(DELIVERY_ID);

    let (claim_result, prune_result) = tokio::join!(store.claim(&id), store.prune_batch(cutoff));

    assert!(matches!(claim_result, Err(DeliveryStoreError::Unavailable)));
    assert!(matches!(prune_result, Err(DeliveryStoreError::Unavailable)));
    sqlx::query("ROLLBACK")
        .execute(&mut *locking_connection)
        .await
        .expect("write lock is released");
}

#[tokio::test]
async fn internal_delivery_store_errors_hide_sqlite_details() {
    let (_directory, pool, store) = test_store().await;
    sqlx::query("DROP TABLE processed_deliveries")
        .execute(&pool)
        .await
        .expect("test table is removed");

    let error = store
        .claim(&delivery_id(DELIVERY_ID))
        .await
        .expect_err("missing table must fail");

    assert!(matches!(error, DeliveryStoreError::Internal));
    for rendered in [error.to_string(), format!("{error:?}")] {
        assert!(!rendered.contains("processed_deliveries"));
        assert!(!rendered.contains("no such table"));
        assert!(!rendered.contains("SqliteError"));
    }
}

#[tokio::test]
async fn prune_batch_respects_fractional_cutoff_seconds() {
    let (_directory, pool, store) = test_store().await;
    for (id, received_at) in [
        (
            "20000000-0000-4000-8000-000000000001",
            "2026-02-01T00:00:00.250Z",
        ),
        (
            "20000000-0000-4000-8000-000000000002",
            "2026-02-01T00:00:00.750Z",
        ),
    ] {
        sqlx::query("INSERT INTO processed_deliveries (delivery_id, received_at) VALUES (?, ?)")
            .bind(id)
            .bind(received_at)
            .execute(&pool)
            .await
            .expect("boundary claim is inserted");
    }
    let cutoff = time::OffsetDateTime::from_unix_timestamp(1_769_904_000)
        .expect("test cutoff timestamp is representable")
        + time::Duration::milliseconds(500);

    assert_eq!(store.prune_batch(cutoff).await.expect("prune succeeds"), 1);
    let retained_id: String =
        sqlx::query_scalar("SELECT delivery_id FROM processed_deliveries ORDER BY delivery_id")
            .fetch_one(&pool)
            .await
            .expect("fresh boundary claim is retained");
    assert_eq!(retained_id, "20000000-0000-4000-8000-000000000002");
}

#[tokio::test]
async fn prune_batch_deletes_at_most_one_thousand_expired_claims() {
    let (_directory, pool, store) = test_store().await;
    sqlx::query(
        "WITH RECURSIVE sequence(value) AS (\
             VALUES(1) UNION ALL SELECT value + 1 FROM sequence WHERE value < 1005\
         )\
         INSERT INTO processed_deliveries (delivery_id, received_at)\
         SELECT printf('00000000-0000-4000-8000-%012d', value),\
                '2026-01-01T00:00:00.000Z'\
         FROM sequence",
    )
    .execute(&pool)
    .await
    .expect("expired claims are inserted");
    for fresh_id in [
        "10000000-0000-4000-8000-000000000001",
        "10000000-0000-4000-8000-000000000002",
    ] {
        sqlx::query("INSERT INTO processed_deliveries (delivery_id, received_at) VALUES (?, ?)")
            .bind(fresh_id)
            .bind("2026-03-01T00:00:00.000Z")
            .execute(&pool)
            .await
            .expect("fresh claim is inserted");
    }
    let cutoff = time::OffsetDateTime::from_unix_timestamp(1_769_904_000)
        .expect("test cutoff timestamp is representable");

    assert_eq!(
        store
            .prune_batch(cutoff)
            .await
            .expect("first prune succeeds"),
        1_000
    );
    assert_eq!(
        store
            .prune_batch(cutoff)
            .await
            .expect("second prune succeeds"),
        5
    );
    assert_eq!(
        store
            .prune_batch(cutoff)
            .await
            .expect("final prune succeeds"),
        0
    );
    let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM processed_deliveries")
        .fetch_one(&pool)
        .await
        .expect("remaining claims are countable");
    assert_eq!(remaining, 2);
}
