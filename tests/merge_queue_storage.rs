use std::time::Duration;

use github_webhook_exporter::{
    domain::{
        merge_queue::{
            PullRequestNumber, QueueCompletion, QueueOutcome, QueueReasonCode, QueueTimestamp,
        },
        repository::RepositoryId,
    },
    storage::{
        open_database, CompletionTransition, EnqueueTransition, MergeQueueStore,
        MergeQueueStoreError,
    },
};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions},
    Row, SqlitePool,
};
use time::OffsetDateTime;

const ENQUEUED_AT: &str = "2026-08-05T10:00:00.125Z";
const COMPLETED_AT: &str = "2026-08-05T10:05:00.875Z";

fn pull_request_number(value: i64) -> PullRequestNumber {
    PullRequestNumber::new(value).expect("test pull-request number is positive")
}

fn timestamp(value: &str) -> QueueTimestamp {
    QueueTimestamp::parse(value).expect("test queue timestamp is valid")
}

async fn test_store() -> (tempfile::TempDir, SqlitePool, MergeQueueStore, RepositoryId) {
    let directory = tempfile::tempdir().expect("temporary directory is created");
    let pool = open_database(&directory.path().join("exporter.sqlite3"))
        .await
        .expect("database opens and migrates");
    let repository_id = insert_repository(&pool).await;
    let store = MergeQueueStore::new(pool.clone());
    (directory, pool, store, repository_id)
}

async fn test_store_with_busy_timeout(
    busy_timeout: Duration,
) -> (tempfile::TempDir, SqlitePool, MergeQueueStore, RepositoryId) {
    let directory = tempfile::tempdir().expect("temporary directory is created");
    let database_path = directory.path().join("exporter.sqlite3");
    let migration_pool = open_database(&database_path)
        .await
        .expect("database opens and migrates");
    migration_pool.close().await;
    let options = SqliteConnectOptions::new()
        .filename(database_path)
        .foreign_keys(true)
        .journal_mode(SqliteJournalMode::Wal)
        .busy_timeout(busy_timeout);
    let pool = SqlitePoolOptions::new()
        .max_connections(2)
        .connect_with(options)
        .await
        .expect("short-timeout test pool opens");
    let repository_id = insert_repository(&pool).await;
    let store = MergeQueueStore::new(pool.clone());
    (directory, pool, store, repository_id)
}

async fn insert_repository(pool: &SqlitePool) -> RepositoryId {
    let id: i64 = sqlx::query_scalar(
        "INSERT INTO repositories (full_name, webhook_secret_ciphertext, webhook_secret_nonce, \
         encryption_version, enabled, created_at, updated_at) \
         VALUES ('owner/repo', X'01', X'02', 1, 1, ?, ?) RETURNING id",
    )
    .bind(ENQUEUED_AT)
    .bind(ENQUEUED_AT)
    .fetch_one(pool)
    .await
    .expect("test repository is inserted");
    RepositoryId::new(id).expect("inserted repository ID is positive")
}

#[test]
fn queue_domain_values_are_validated_and_bounded() {
    assert_eq!(PullRequestNumber::new(1).expect("one is positive").get(), 1);
    assert!(PullRequestNumber::new(0).is_err());
    assert!(PullRequestNumber::new(-1).is_err());

    let normalized = QueueTimestamp::parse("2026-08-05T12:30:45.123456+02:00")
        .expect("RFC 3339 timestamp is accepted");
    assert_eq!(normalized.as_str(), "2026-08-05T10:30:45.123Z");
    assert!(!format!("{normalized:?}").contains("2026-08-05"));
    for invalid in ["", "2026-08-05", "not-a-timestamp"] {
        assert!(
            QueueTimestamp::parse(invalid).is_err(),
            "accepted {invalid}"
        );
    }

    for (value, expected) in [
        ("pending", QueueOutcome::Pending),
        ("succeeded", QueueOutcome::Succeeded),
        ("failed", QueueOutcome::Failed),
        ("cancelled", QueueOutcome::Cancelled),
        ("unknown", QueueOutcome::Unknown),
    ] {
        assert_eq!(QueueOutcome::parse(value), Ok(expected));
        assert_eq!(expected.as_str(), value);
    }
    assert!(QueueOutcome::parse("attacker-controlled").is_err());

    for (value, expected) in [
        ("none", QueueReasonCode::None),
        ("pull_request_merged", QueueReasonCode::PullRequestMerged),
        ("unclassified_dequeue", QueueReasonCode::UnclassifiedDequeue),
    ] {
        assert_eq!(QueueReasonCode::parse(value), Ok(expected));
        assert_eq!(expected.as_str(), value);
    }
    assert!(QueueReasonCode::parse("raw-dequeue-reason").is_err());

    let completed_at = timestamp(COMPLETED_AT);
    let merged = QueueCompletion::pull_request_merged(completed_at.clone());
    assert_eq!(merged.outcome(), QueueOutcome::Succeeded);
    assert_eq!(merged.reason_code(), QueueReasonCode::PullRequestMerged);
    assert_eq!(merged.completed_at(), &completed_at);
    let dequeued = QueueCompletion::unclassified_dequeue(completed_at);
    assert_eq!(dequeued.outcome(), QueueOutcome::Unknown);
    assert_eq!(dequeued.reason_code(), QueueReasonCode::UnclassifiedDequeue);
}

#[tokio::test]
async fn merge_queue_migration_matches_the_bounded_schema() {
    let (_directory, pool, _store, repository_id) = test_store().await;

    let columns = sqlx::query("PRAGMA table_info(merge_queue_attempts)")
        .fetch_all(&pool)
        .await
        .expect("queue schema is inspectable")
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
            ("repository_id".to_owned(), "INTEGER".to_owned(), 1, 0),
            ("pull_request_number".to_owned(), "INTEGER".to_owned(), 1, 0,),
            ("enqueued_at".to_owned(), "TEXT".to_owned(), 1, 0),
            ("completed_at".to_owned(), "TEXT".to_owned(), 0, 0),
            ("outcome".to_owned(), "TEXT".to_owned(), 1, 0),
            ("reason_code".to_owned(), "TEXT".to_owned(), 1, 0),
        ]
    );
    let column_names = columns
        .iter()
        .map(|column| column.0.as_str())
        .collect::<Vec<_>>();
    for forbidden in [
        "payload",
        "repository_name",
        "delivery_uuid",
        "signature",
        "sha",
        "raw_dequeue_reason",
        "secret",
    ] {
        assert!(!column_names.contains(&forbidden));
    }

    let foreign_key = sqlx::query("PRAGMA foreign_key_list(merge_queue_attempts)")
        .fetch_one(&pool)
        .await
        .expect("queue foreign key is inspectable");
    assert_eq!(foreign_key.get::<String, _>("table"), "repositories");
    assert_eq!(foreign_key.get::<String, _>("from"), "repository_id");
    assert_eq!(foreign_key.get::<String, _>("to"), "id");
    assert_eq!(foreign_key.get::<String, _>("on_delete"), "CASCADE");

    let indexes = sqlx::query("PRAGMA index_list(merge_queue_attempts)")
        .fetch_all(&pool)
        .await
        .expect("queue indexes are inspectable");
    let active_index = indexes
        .iter()
        .find(|row| row.get::<String, _>("name") == "one_active_merge_queue_attempt")
        .expect("partial active-attempt index exists");
    assert_eq!(active_index.get::<i64, _>("unique"), 1);
    assert_eq!(active_index.get::<i64, _>("partial"), 1);
    let active_columns = sqlx::query("PRAGMA index_info(one_active_merge_queue_attempt)")
        .fetch_all(&pool)
        .await
        .expect("active index columns are inspectable")
        .into_iter()
        .map(|row| row.get::<String, _>("name"))
        .collect::<Vec<_>>();
    assert_eq!(active_columns, vec!["repository_id", "pull_request_number"]);
    let active_sql: String = sqlx::query_scalar(
        "SELECT sql FROM sqlite_master WHERE type = 'index' \
         AND name = 'one_active_merge_queue_attempt'",
    )
    .fetch_one(&pool)
    .await
    .expect("active index SQL is readable");
    assert!(active_sql.contains("WHERE completed_at IS NULL"));
    let completed_columns = sqlx::query("PRAGMA index_info(merge_queue_attempts_completed_at_idx)")
        .fetch_all(&pool)
        .await
        .expect("completed index is inspectable")
        .into_iter()
        .map(|row| row.get::<String, _>("name"))
        .collect::<Vec<_>>();
    assert_eq!(completed_columns, vec!["completed_at"]);

    for statement in [
        format!(
            "INSERT INTO merge_queue_attempts \
             (repository_id, pull_request_number, enqueued_at, completed_at, outcome, reason_code) \
             VALUES ({}, 0, '{ENQUEUED_AT}', NULL, 'pending', 'none')",
            repository_id.get()
        ),
        format!(
            "INSERT INTO merge_queue_attempts \
             (repository_id, pull_request_number, enqueued_at, completed_at, outcome, reason_code) \
             VALUES ({}, 1, '{ENQUEUED_AT}', NULL, 'invented', 'none')",
            repository_id.get()
        ),
        format!(
            "INSERT INTO merge_queue_attempts \
             (repository_id, pull_request_number, enqueued_at, completed_at, outcome, reason_code) \
             VALUES ({}, 1, '{ENQUEUED_AT}', '{COMPLETED_AT}', 'pending', 'none')",
            repository_id.get()
        ),
        format!(
            "INSERT INTO merge_queue_attempts \
             (repository_id, pull_request_number, enqueued_at, completed_at, outcome, reason_code) \
             VALUES ({}, 1, '{ENQUEUED_AT}', NULL, 'succeeded', 'pull_request_merged')",
            repository_id.get()
        ),
    ] {
        assert!(
            sqlx::query(&statement).execute(&pool).await.is_err(),
            "invalid row was accepted"
        );
    }
}

#[tokio::test]
async fn sequential_transitions_are_exact_and_idempotent() {
    let (_directory, pool, store, repository_id) = test_store().await;
    let number = pull_request_number(42);
    let enqueued_at = timestamp(ENQUEUED_AT);

    assert_eq!(
        store
            .enqueue(repository_id, number, &enqueued_at)
            .await
            .expect("first enqueue succeeds"),
        EnqueueTransition::Created
    );
    assert_eq!(
        store
            .enqueue(repository_id, number, &timestamp("2026-08-05T10:01:00Z"))
            .await
            .expect("repeated enqueue succeeds"),
        EnqueueTransition::AlreadyActive
    );
    let pending = sqlx::query(
        "SELECT enqueued_at, completed_at, outcome, reason_code \
         FROM merge_queue_attempts WHERE repository_id = ? AND pull_request_number = ?",
    )
    .bind(repository_id.get())
    .bind(number.get())
    .fetch_one(&pool)
    .await
    .expect("pending attempt is readable");
    assert_eq!(pending.get::<String, _>("enqueued_at"), ENQUEUED_AT);
    assert_eq!(pending.get::<Option<String>, _>("completed_at"), None);
    assert_eq!(pending.get::<String, _>("outcome"), "pending");
    assert_eq!(pending.get::<String, _>("reason_code"), "none");

    let merged = QueueCompletion::pull_request_merged(timestamp(COMPLETED_AT));
    let completed_transition = store
        .complete(repository_id, number, &merged)
        .await
        .expect("merged completion succeeds");
    assert_eq!(
        completed_transition,
        CompletionTransition::Completed {
            enqueued_at: timestamp(ENQUEUED_AT),
        }
    );
    assert_eq!(
        store
            .complete(repository_id, number, &merged)
            .await
            .expect("completion replay succeeds"),
        CompletionTransition::AlreadyCompleted
    );
    let completed = sqlx::query(
        "SELECT completed_at, outcome, reason_code FROM merge_queue_attempts \
         WHERE repository_id = ? AND pull_request_number = ?",
    )
    .bind(repository_id.get())
    .bind(number.get())
    .fetch_one(&pool)
    .await
    .expect("completed attempt is readable");
    assert_eq!(completed.get::<String, _>("completed_at"), COMPLETED_AT);
    assert_eq!(completed.get::<String, _>("outcome"), "succeeded");
    assert_eq!(
        completed.get::<String, _>("reason_code"),
        "pull_request_merged"
    );

    assert_eq!(
        store
            .enqueue(repository_id, number, &timestamp("2026-08-05T11:00:00Z"))
            .await
            .expect("new attempt after completion succeeds"),
        EnqueueTransition::Created
    );
    let dequeued = QueueCompletion::unclassified_dequeue(timestamp("2026-08-05T11:02:00Z"));
    assert_eq!(
        store
            .complete(repository_id, number, &dequeued)
            .await
            .expect("dequeue completion succeeds"),
        CompletionTransition::Completed {
            enqueued_at: timestamp("2026-08-05T11:00:00Z"),
        }
    );
    let outcomes = sqlx::query(
        "SELECT outcome, reason_code FROM merge_queue_attempts \
         WHERE repository_id = ? AND pull_request_number = ? ORDER BY id",
    )
    .bind(repository_id.get())
    .bind(number.get())
    .fetch_all(&pool)
    .await
    .expect("attempt history is readable");
    assert_eq!(outcomes.len(), 2);
    assert_eq!(outcomes[1].get::<String, _>("outcome"), "unknown");
    assert_eq!(
        outcomes[1].get::<String, _>("reason_code"),
        "unclassified_dequeue"
    );

    assert_eq!(
        store
            .complete(repository_id, pull_request_number(404), &merged)
            .await
            .expect("missing completion is an expected no-op"),
        CompletionTransition::MissingActiveAttempt
    );
}

#[tokio::test]
async fn concurrent_enqueues_leave_exactly_one_active_attempt() {
    let (_directory, pool, store, repository_id) = test_store().await;
    let mut tasks = tokio::task::JoinSet::new();

    for _ in 0..16 {
        let concurrent_store = store.clone();
        let enqueued_at = timestamp(ENQUEUED_AT);
        tasks.spawn(async move {
            concurrent_store
                .enqueue(repository_id, pull_request_number(42), &enqueued_at)
                .await
        });
    }

    let mut created = 0;
    let mut already_active = 0;
    while let Some(result) = tasks.join_next().await {
        match result
            .expect("enqueue task completes")
            .expect("concurrent enqueue succeeds")
        {
            EnqueueTransition::Created => created += 1,
            EnqueueTransition::AlreadyActive => already_active += 1,
        }
    }
    assert_eq!(created, 1);
    assert_eq!(already_active, 15);
    let active_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM merge_queue_attempts WHERE repository_id = ? \
         AND pull_request_number = 42 AND completed_at IS NULL",
    )
    .bind(repository_id.get())
    .fetch_one(&pool)
    .await
    .expect("active attempts are countable");
    assert_eq!(active_count, 1);
}

#[tokio::test]
async fn concurrent_completions_apply_exactly_one_terminal_transition() {
    let (_directory, pool, store, repository_id) = test_store().await;
    let number = pull_request_number(42);
    store
        .enqueue(repository_id, number, &timestamp(ENQUEUED_AT))
        .await
        .expect("attempt is enqueued");
    let mut tasks = tokio::task::JoinSet::new();

    for index in 0..16 {
        let concurrent_store = store.clone();
        let completion = if index % 2 == 0 {
            QueueCompletion::pull_request_merged(timestamp(COMPLETED_AT))
        } else {
            QueueCompletion::unclassified_dequeue(timestamp(COMPLETED_AT))
        };
        tasks.spawn(async move {
            concurrent_store
                .complete(repository_id, number, &completion)
                .await
        });
    }

    let mut completed = 0;
    let mut already_completed = 0;
    while let Some(result) = tasks.join_next().await {
        match result
            .expect("completion task completes")
            .expect("concurrent completion succeeds")
        {
            CompletionTransition::Completed { .. } => completed += 1,
            CompletionTransition::AlreadyCompleted => already_completed += 1,
            CompletionTransition::MissingActiveAttempt => {
                panic!("existing attempt was reported missing")
            }
        }
    }
    assert_eq!(completed, 1);
    assert_eq!(already_completed, 15);
    let row = sqlx::query(
        "SELECT outcome, reason_code, completed_at FROM merge_queue_attempts \
         WHERE repository_id = ? AND pull_request_number = ?",
    )
    .bind(repository_id.get())
    .bind(number.get())
    .fetch_one(&pool)
    .await
    .expect("completed row is readable");
    let terminal_pair = (
        row.get::<String, _>("outcome"),
        row.get::<String, _>("reason_code"),
    );
    assert!(matches!(
        terminal_pair,
        (ref outcome, ref reason)
            if (outcome == "succeeded" && reason == "pull_request_merged")
                || (outcome == "unknown" && reason == "unclassified_dequeue")
    ));
    assert_eq!(row.get::<String, _>("completed_at"), COMPLETED_AT);
}

#[tokio::test]
async fn pending_attempt_survives_reopen_and_completes_later() {
    let directory = tempfile::tempdir().expect("temporary directory is created");
    let database_path = directory.path().join("exporter.sqlite3");
    let pool = open_database(&database_path)
        .await
        .expect("database opens and migrates");
    let repository_id = insert_repository(&pool).await;
    let store = MergeQueueStore::new(pool.clone());
    let number = pull_request_number(42);
    store
        .enqueue(repository_id, number, &timestamp(ENQUEUED_AT))
        .await
        .expect("attempt is enqueued");
    drop(store);
    pool.close().await;

    let reopened_pool = open_database(&database_path)
        .await
        .expect("database reopens");
    let reopened_store = MergeQueueStore::new(reopened_pool);
    assert_eq!(
        reopened_store
            .complete(
                repository_id,
                number,
                &QueueCompletion::pull_request_merged(timestamp(COMPLETED_AT)),
            )
            .await
            .expect("persisted attempt completes"),
        CompletionTransition::Completed {
            enqueued_at: timestamp(ENQUEUED_AT),
        }
    );
}

#[tokio::test]
async fn failed_completion_rolls_back_the_pending_attempt() {
    let (_directory, pool, store, repository_id) = test_store().await;
    let number = pull_request_number(42);
    store
        .enqueue(repository_id, number, &timestamp(ENQUEUED_AT))
        .await
        .expect("attempt is enqueued");
    sqlx::query(
        "CREATE TRIGGER reject_queue_completion BEFORE UPDATE ON merge_queue_attempts \
         BEGIN SELECT RAISE(ABORT, 'sensitive-forced-failure'); END",
    )
    .execute(&pool)
    .await
    .expect("failure trigger is installed");

    let error = store
        .complete(
            repository_id,
            number,
            &QueueCompletion::pull_request_merged(timestamp(COMPLETED_AT)),
        )
        .await
        .expect_err("forced completion fails");

    assert_eq!(error, MergeQueueStoreError::Internal);
    let row = sqlx::query(
        "SELECT completed_at, outcome, reason_code FROM merge_queue_attempts \
         WHERE repository_id = ? AND pull_request_number = ?",
    )
    .bind(repository_id.get())
    .bind(number.get())
    .fetch_one(&pool)
    .await
    .expect("pending row remains readable");
    assert_eq!(row.get::<Option<String>, _>("completed_at"), None);
    assert_eq!(row.get::<String, _>("outcome"), "pending");
    assert_eq!(row.get::<String, _>("reason_code"), "none");
}

#[tokio::test]
async fn repository_deletion_cascades_to_all_attempts() {
    let (_directory, pool, store, repository_id) = test_store().await;
    for number in [1, 2] {
        store
            .enqueue(
                repository_id,
                pull_request_number(number),
                &timestamp(ENQUEUED_AT),
            )
            .await
            .expect("attempt is enqueued");
    }
    store
        .complete(
            repository_id,
            pull_request_number(1),
            &QueueCompletion::pull_request_merged(timestamp(COMPLETED_AT)),
        )
        .await
        .expect("first attempt completes");

    sqlx::query("DELETE FROM repositories WHERE id = ?")
        .bind(repository_id.get())
        .execute(&pool)
        .await
        .expect("repository is deleted");

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM merge_queue_attempts")
        .fetch_one(&pool)
        .await
        .expect("remaining attempts are countable");
    assert_eq!(count, 0);
}

#[tokio::test]
async fn prune_completed_batch_is_bounded_and_preserves_pending_and_fresh_attempts() {
    let (_directory, pool, store, repository_id) = test_store().await;
    sqlx::query(
        "WITH RECURSIVE sequence(value) AS (\
             VALUES(1) UNION ALL SELECT value + 1 FROM sequence WHERE value < 1005\
         )\
         INSERT INTO merge_queue_attempts \
             (repository_id, pull_request_number, enqueued_at, completed_at, outcome, reason_code)\
         SELECT ?, value, '2026-01-01T00:00:00.000Z', '2026-01-02T00:00:00.000Z',\
                'unknown', 'unclassified_dequeue' FROM sequence",
    )
    .bind(repository_id.get())
    .execute(&pool)
    .await
    .expect("expired attempts are inserted");
    sqlx::query(
        "INSERT INTO merge_queue_attempts \
         (repository_id, pull_request_number, enqueued_at, completed_at, outcome, reason_code) \
         VALUES (?, 2001, '2026-03-01T00:00:00.000Z', '2026-03-02T00:00:00.000Z', \
                 'succeeded', 'pull_request_merged'), \
                (?, 2002, '2026-03-01T00:00:00.000Z', NULL, 'pending', 'none')",
    )
    .bind(repository_id.get())
    .bind(repository_id.get())
    .execute(&pool)
    .await
    .expect("fresh and pending attempts are inserted");
    let cutoff =
        OffsetDateTime::from_unix_timestamp(1_769_904_000).expect("test cutoff is representable");

    assert_eq!(
        store
            .prune_completed_batch(cutoff)
            .await
            .expect("first prune succeeds"),
        1_000
    );
    assert_eq!(
        store
            .prune_completed_batch(cutoff)
            .await
            .expect("second prune succeeds"),
        5
    );
    assert_eq!(
        store
            .prune_completed_batch(cutoff)
            .await
            .expect("final prune succeeds"),
        0
    );
    let retained_numbers: Vec<i64> = sqlx::query_scalar(
        "SELECT pull_request_number FROM merge_queue_attempts ORDER BY pull_request_number",
    )
    .fetch_all(&pool)
    .await
    .expect("retained attempts are readable");
    assert_eq!(retained_numbers, vec![2001, 2002]);
}

#[tokio::test]
async fn locked_and_internal_errors_are_typed_and_redacted() {
    let (_directory, pool, store, repository_id) =
        test_store_with_busy_timeout(Duration::from_millis(10)).await;
    let mut locking_connection = pool
        .acquire()
        .await
        .expect("locking connection is available");
    sqlx::query("BEGIN IMMEDIATE")
        .execute(&mut *locking_connection)
        .await
        .expect("write lock is acquired");

    let error = store
        .enqueue(
            repository_id,
            pull_request_number(42),
            &timestamp(ENQUEUED_AT),
        )
        .await
        .expect_err("locked enqueue fails");

    assert_eq!(error, MergeQueueStoreError::Unavailable);
    sqlx::query("ROLLBACK")
        .execute(&mut *locking_connection)
        .await
        .expect("write lock is released");

    sqlx::query("DROP TABLE merge_queue_attempts")
        .execute(&pool)
        .await
        .expect("queue table is removed");
    let internal = store
        .enqueue(
            repository_id,
            pull_request_number(42),
            &timestamp(ENQUEUED_AT),
        )
        .await
        .expect_err("missing table fails");
    assert_eq!(internal, MergeQueueStoreError::Internal);
    for rendered in [internal.to_string(), format!("{internal:?}")] {
        assert!(!rendered.contains("merge_queue_attempts"));
        assert!(!rendered.contains("no such table"));
        assert!(!rendered.contains("SqliteError"));
    }
}
