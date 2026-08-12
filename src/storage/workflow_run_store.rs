use std::fmt;

use sqlx::{FromRow, SqlitePool};
use thiserror::Error;
use time::{format_description::FormatItem, macros::format_description, OffsetDateTime, UtcOffset};

use crate::{
    domain::repository::RepositoryId,
    telemetry::{
        trace::{self, DatabaseOperation},
        workflow::{
            WorkflowBranch, WorkflowEvent, WorkflowRunAttempt, WorkflowRunContext, WorkflowRunId,
        },
    },
};

use super::sqlite_is_busy_or_locked;

const WORKFLOW_RUN_PRUNE_BATCH_SIZE: i64 = 1_000;
const WORKFLOW_RUN_TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// Durable bounded metadata used to correlate workflow runs and completed jobs.
#[derive(Clone)]
pub(crate) struct WorkflowRunStore {
    pool: SqlitePool,
}

impl WorkflowRunStore {
    /// Creates a workflow-run context store from an already migrated SQLite pool.
    pub(crate) fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Inserts or replaces bounded context for one repository workflow-run attempt.
    pub(crate) async fn upsert(
        &self,
        repository_id: RepositoryId,
        context: &WorkflowRunContext,
    ) -> Result<(), WorkflowRunStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::WorkflowRunUpsert,
            self.upsert_inner(repository_id, context),
        )
        .await
    }

    async fn upsert_inner(
        &self,
        repository_id: RepositoryId,
        context: &WorkflowRunContext,
    ) -> Result<(), WorkflowRunStoreError> {
        sqlx::query(
            "INSERT INTO workflow_run_contexts \
             (repository_id, workflow_run_id, workflow_run_attempt, event, source_branch, \
              target_branch, updated_at) \
             VALUES (?, ?, ?, ?, ?, ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now')) \
             ON CONFLICT(repository_id, workflow_run_id, workflow_run_attempt) DO UPDATE SET \
             event = excluded.event, source_branch = excluded.source_branch, \
             target_branch = excluded.target_branch, updated_at = excluded.updated_at",
        )
        .bind(repository_id.get())
        .bind(context.run_id().get())
        .bind(context.run_attempt().get())
        .bind(context.event().as_str())
        .bind(context.source_branch().map(WorkflowBranch::as_str))
        .bind(context.target_branch().map(WorkflowBranch::as_str))
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(())
    }

    /// Loads bounded context for one repository workflow-run attempt.
    pub(crate) async fn get(
        &self,
        repository_id: RepositoryId,
        run_id: WorkflowRunId,
        run_attempt: WorkflowRunAttempt,
    ) -> Result<Option<WorkflowRunContext>, WorkflowRunStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::WorkflowRunGet,
            self.get_inner(repository_id, run_id, run_attempt),
        )
        .await
    }

    async fn get_inner(
        &self,
        repository_id: RepositoryId,
        run_id: WorkflowRunId,
        run_attempt: WorkflowRunAttempt,
    ) -> Result<Option<WorkflowRunContext>, WorkflowRunStoreError> {
        let row = sqlx::query_as::<_, StoredWorkflowRunContext>(
            "SELECT event, source_branch, target_branch FROM workflow_run_contexts \
             WHERE repository_id = ? AND workflow_run_id = ? AND workflow_run_attempt = ?",
        )
        .bind(repository_id.get())
        .bind(run_id.get())
        .bind(run_attempt.get())
        .fetch_optional(&self.pool)
        .await
        .map_err(map_sqlx_error)?;

        row.map(|row| row.into_context(run_id, run_attempt))
            .transpose()
    }

    /// Deletes at most 1,000 workflow-run contexts last updated before `cutoff`.
    pub(crate) async fn prune_batch(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<u64, WorkflowRunStoreError> {
        trace::instrument_database_operation(
            DatabaseOperation::WorkflowRunPrune,
            self.prune_batch_inner(cutoff),
        )
        .await
    }

    async fn prune_batch_inner(
        &self,
        cutoff: OffsetDateTime,
    ) -> Result<u64, WorkflowRunStoreError> {
        let cutoff = cutoff
            .to_offset(UtcOffset::UTC)
            .format(WORKFLOW_RUN_TIMESTAMP_FORMAT)
            .map_err(|_| WorkflowRunStoreError::Internal)?;
        let result = sqlx::query(
            "DELETE FROM workflow_run_contexts WHERE rowid IN (\
                 SELECT rowid FROM workflow_run_contexts WHERE updated_at < ? \
                 ORDER BY updated_at, rowid LIMIT ?\
             )",
        )
        .bind(cutoff)
        .bind(WORKFLOW_RUN_PRUNE_BATCH_SIZE)
        .execute(&self.pool)
        .await
        .map_err(map_sqlx_error)?;
        Ok(result.rows_affected())
    }
}

#[derive(FromRow)]
struct StoredWorkflowRunContext {
    event: String,
    source_branch: Option<String>,
    target_branch: Option<String>,
}

impl StoredWorkflowRunContext {
    fn into_context(
        self,
        run_id: WorkflowRunId,
        run_attempt: WorkflowRunAttempt,
    ) -> Result<WorkflowRunContext, WorkflowRunStoreError> {
        let event = match self.event.as_str() {
            "pull_request" => WorkflowEvent::PullRequest,
            "merge_group" => WorkflowEvent::MergeGroup,
            "push" => WorkflowEvent::Push,
            "other" => WorkflowEvent::Other,
            _ => return Err(WorkflowRunStoreError::Internal),
        };
        let source_branch = validated_stored_branch(self.source_branch.as_deref())?;
        let target_branch = validated_stored_branch(self.target_branch.as_deref())?;
        Ok(WorkflowRunContext::new(
            run_id,
            run_attempt,
            event,
            source_branch,
            target_branch,
        ))
    }
}

fn validated_stored_branch(
    value: Option<&str>,
) -> Result<Option<WorkflowBranch>, WorkflowRunStoreError> {
    value
        .map(|value| WorkflowBranch::sanitize(value).ok_or(WorkflowRunStoreError::Internal))
        .transpose()
}

/// A stable, redacted workflow-run context persistence failure.
#[derive(Clone, Copy, Error, PartialEq, Eq)]
pub(crate) enum WorkflowRunStoreError {
    /// SQLite is temporarily busy or locked.
    #[error("workflow-run storage is temporarily unavailable")]
    Unavailable,
    /// SQLite returned an unexpected persistence failure whose details were discarded.
    #[error("internal workflow-run persistence failure")]
    Internal,
}

impl fmt::Debug for WorkflowRunStoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Unavailable => "WorkflowRunStoreError::Unavailable",
            Self::Internal => "WorkflowRunStoreError::Internal",
        })
    }
}

fn map_sqlx_error(error: sqlx::Error) -> WorkflowRunStoreError {
    if sqlite_is_busy_or_locked(&error) {
        WorkflowRunStoreError::Unavailable
    } else {
        WorkflowRunStoreError::Internal
    }
}

#[cfg(test)]
mod tests {
    use sqlx::{Row, SqlitePool};
    use time::macros::datetime;

    use super::WorkflowRunStore;
    use crate::{
        domain::repository::RepositoryId,
        storage::open_database,
        telemetry::workflow::{
            WorkflowBranch, WorkflowEvent, WorkflowRunAttempt, WorkflowRunContext, WorkflowRunId,
        },
    };

    fn context(run_attempt: i64, target: &str) -> WorkflowRunContext {
        WorkflowRunContext::new(
            WorkflowRunId::new(31).expect("run id is positive"),
            WorkflowRunAttempt::new(run_attempt).expect("attempt is positive"),
            WorkflowEvent::MergeGroup,
            WorkflowBranch::sanitize("gh-readonly-queue/main/pr-7"),
            WorkflowBranch::sanitize(target),
        )
    }

    async fn insert_repository(pool: &SqlitePool) -> RepositoryId {
        let id = sqlx::query_scalar(
            "INSERT INTO repositories (full_name, webhook_secret_ciphertext, \
             webhook_secret_nonce, encryption_version, enabled, created_at, updated_at) \
             VALUES ('owner/repo', X'01', X'02', 1, 1, \
             '2026-08-12T10:00:00.000Z', '2026-08-12T10:00:00.000Z') RETURNING id",
        )
        .fetch_one(pool)
        .await
        .expect("repository inserts");
        RepositoryId::new(id).expect("repository id is positive")
    }

    #[tokio::test]
    async fn migration_contains_only_bounded_correlation_fields() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens");

        let columns = sqlx::query("PRAGMA table_info(workflow_run_contexts)")
            .fetch_all(&pool)
            .await
            .expect("schema is inspectable")
            .into_iter()
            .map(|row| row.get::<String, _>("name"))
            .collect::<Vec<_>>();
        assert_eq!(
            columns,
            [
                "repository_id",
                "workflow_run_id",
                "workflow_run_attempt",
                "event",
                "source_branch",
                "target_branch",
                "updated_at",
            ]
        );
        for forbidden in ["payload", "signature", "actor", "html_url", "logs"] {
            assert!(!columns.iter().any(|column| column == forbidden));
        }
    }

    #[tokio::test]
    async fn upsert_get_survives_reopen_and_keeps_attempts_independent() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let path = directory.path().join("exporter.sqlite3");
        let pool = open_database(&path).await.expect("database opens");
        let repository_id = insert_repository(&pool).await;
        let store = WorkflowRunStore::new(pool.clone());

        store
            .upsert(repository_id, &context(1, "main"))
            .await
            .expect("first attempt persists");
        store
            .upsert(repository_id, &context(2, "release"))
            .await
            .expect("second attempt persists");
        pool.close().await;

        let reopened = open_database(&path).await.expect("database reopens");
        let store = WorkflowRunStore::new(reopened);
        let first = store
            .get(
                repository_id,
                WorkflowRunId::new(31).expect("run id is positive"),
                WorkflowRunAttempt::new(1).expect("attempt is positive"),
            )
            .await
            .expect("lookup succeeds")
            .expect("first attempt exists");
        let second = store
            .get(
                repository_id,
                WorkflowRunId::new(31).expect("run id is positive"),
                WorkflowRunAttempt::new(2).expect("attempt is positive"),
            )
            .await
            .expect("lookup succeeds")
            .expect("second attempt exists");

        assert_eq!(
            first.target_branch().map(|value| value.as_str()),
            Some("main")
        );
        assert_eq!(
            second.target_branch().map(|value| value.as_str()),
            Some("release")
        );
    }

    #[tokio::test]
    async fn upsert_replaces_only_bounded_context_and_prune_uses_update_time() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens");
        let repository_id = insert_repository(&pool).await;
        let store = WorkflowRunStore::new(pool.clone());

        store
            .upsert(repository_id, &context(1, "main"))
            .await
            .expect("context persists");
        sqlx::query("UPDATE workflow_run_contexts SET updated_at = '2026-08-12T10:00:00.000Z'")
            .execute(&pool)
            .await
            .expect("fixture timestamp updates");

        assert_eq!(
            store
                .prune_batch(datetime!(2026-08-12 10:00:01 UTC))
                .await
                .expect("prune succeeds"),
            1
        );
        assert!(store
            .get(
                repository_id,
                WorkflowRunId::new(31).expect("run id is positive"),
                WorkflowRunAttempt::new(1).expect("attempt is positive"),
            )
            .await
            .expect("lookup succeeds")
            .is_none());
    }

    #[tokio::test]
    async fn prune_is_bounded_to_one_thousand_rows_per_call() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens");
        let _repository_id = insert_repository(&pool).await;
        let store = WorkflowRunStore::new(pool.clone());
        sqlx::query(
            "WITH RECURSIVE sequence(value) AS (\
                 VALUES(1) UNION ALL SELECT value + 1 FROM sequence WHERE value < 1005\
             ) INSERT INTO workflow_run_contexts \
             (repository_id, workflow_run_id, workflow_run_attempt, event, updated_at) \
             SELECT 1, value, 1, 'other', '2020-01-01T00:00:00.000Z' FROM sequence",
        )
        .execute(&pool)
        .await
        .expect("expired contexts insert");

        assert_eq!(
            store
                .prune_batch(datetime!(2026-08-12 10:00:00 UTC))
                .await
                .expect("first prune succeeds"),
            1_000
        );
        assert_eq!(
            store
                .prune_batch(datetime!(2026-08-12 10:00:00 UTC))
                .await
                .expect("second prune succeeds"),
            5
        );
    }

    #[tokio::test]
    async fn stored_branch_is_resanitized_without_failing_the_job_lookup() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens");
        let repository_id = insert_repository(&pool).await;
        let store = WorkflowRunStore::new(pool.clone());
        store
            .upsert(repository_id, &context(1, "main"))
            .await
            .expect("context persists");
        sqlx::query(
            "UPDATE workflow_run_contexts SET source_branch = ? \
             WHERE repository_id = ? AND workflow_run_id = 31 AND workflow_run_attempt = 1",
        )
        .bind("feature\nbranch")
        .bind(repository_id.get())
        .execute(&pool)
        .await
        .expect("legacy branch fixture updates");

        let loaded = store
            .get(
                repository_id,
                WorkflowRunId::new(31).expect("run id is positive"),
                WorkflowRunAttempt::new(1).expect("attempt is positive"),
            )
            .await
            .expect("lookup degrades safely")
            .expect("context exists");

        assert_eq!(
            loaded.source_branch().map(WorkflowBranch::as_str),
            Some("featurebranch")
        );
    }

    #[tokio::test]
    async fn unknown_repository_failure_is_redacted() {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens");
        let store = WorkflowRunStore::new(pool);

        let error = store
            .upsert(
                RepositoryId::new(999).expect("repository id is positive"),
                &context(1, "main"),
            )
            .await
            .expect_err("unknown repository fails");

        assert_eq!(format!("{error:?}"), "WorkflowRunStoreError::Internal");
        assert!(!format!("{error}").contains("FOREIGN KEY"));
    }
}
