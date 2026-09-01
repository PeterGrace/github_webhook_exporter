use async_trait::async_trait;
use time::OffsetDateTime;
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

use crate::{
    domain::repository::RepositoryId,
    lifecycle,
    security::CanonicalRepositoryName,
    storage::RequiredCheckStore,
    telemetry::{
        workflow::{RequiredChecks, WorkflowBranch},
        LOCAL_ONLY_LOG_TARGET,
    },
};

use super::client::{GitHubAppClient, GitHubClientError};

/// Bounded number of pending refresh requests.
///
/// The queue exists to decouple the webhook path from GitHub, not to buffer a backlog. When it is
/// full the newest request is dropped: the branch simply stays "unknown" until the next webhook
/// asks again, which is exactly the degradation the cache-only hot path is designed for.
const REFRESH_QUEUE_CAPACITY: usize = 64;

/// One request to refresh the cached required checks of a repository branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RequiredCheckRefreshRequest {
    repository_id: RepositoryId,
    repository_name: CanonicalRepositoryName,
    branch: WorkflowBranch,
}

impl RequiredCheckRefreshRequest {
    /// Creates a refresh request for one repository branch.
    pub(crate) fn new(
        repository_id: RepositoryId,
        repository_name: CanonicalRepositoryName,
        branch: WorkflowBranch,
    ) -> Self {
        Self {
            repository_id,
            repository_name,
            branch,
        }
    }
}

/// The webhook path's non-blocking handle onto the out-of-band refresher.
#[derive(Clone, Debug)]
pub(crate) struct RequiredCheckRefreshHandle(mpsc::Sender<RequiredCheckRefreshRequest>);

impl RequiredCheckRefreshHandle {
    /// Asks the refresher to look up one repository branch, without ever blocking the caller.
    ///
    /// This is called from inside the webhook handler, before the acknowledgement is written, so it
    /// must never await. A full queue drops the request rather than applying backpressure to an
    /// inbound GitHub delivery.
    pub(crate) fn request(&self, request: RequiredCheckRefreshRequest) {
        if self.0.try_send(request).is_err() {
            // This is the only log in this module emitted from the webhook handler rather than
            // from the background task, so it needs both markers the rest of the module gets for
            // free: the local-only target keeps a dropped refresh out of exported logs, and
            // `parent: None` keeps it off the live request trace it would otherwise attach to.
            debug!(
                target: LOCAL_ONLY_LOG_TARGET,
                parent: None,
                outcome = "queue_full",
                "required-check refresh request dropped"
            );
        }
    }
}

/// Reads the branch-protection required checks of one repository branch.
///
/// [`GitHubAppClient`] is the only production implementation. The trait exists so the refresher's
/// orchestration — the dedup skip and the three failure outcomes — is reachable from tests without
/// a network or a private key.
#[async_trait]
pub(crate) trait RequiredCheckSource: Send {
    /// Returns the required check names for `branch`, or a bounded outbound failure.
    async fn required_checks(
        &mut self,
        repository: &CanonicalRepositoryName,
        branch: &WorkflowBranch,
        now: OffsetDateTime,
    ) -> Result<RequiredChecks, GitHubClientError>;
}

#[async_trait]
impl RequiredCheckSource for GitHubAppClient {
    async fn required_checks(
        &mut self,
        repository: &CanonicalRepositoryName,
        branch: &WorkflowBranch,
        now: OffsetDateTime,
    ) -> Result<RequiredChecks, GitHubClientError> {
        GitHubAppClient::required_checks(self, repository, branch, now).await
    }
}

/// The background task that fills the required-check cache from the GitHub API.
///
/// Owning the only [`GitHubAppClient`] keeps the installation token in exactly one place, so a
/// burst of refreshes shares one minted token.
pub struct RequiredCheckRefresher {
    // Boxed rather than a type parameter: `RequiredCheckRefresher` is part of the public
    // `BackgroundServices` surface, and a generic would push the parameter out through `app.rs`
    // and `main.rs` for no benefit. One virtual call per refresh is free at this rate.
    source: Box<dyn RequiredCheckSource>,
    store: RequiredCheckStore,
    requests: mpsc::Receiver<RequiredCheckRefreshRequest>,
}

impl RequiredCheckRefresher {
    /// Creates a refresher and the handle the webhook path uses to reach it.
    ///
    /// # Parameters
    ///
    /// * `client` - The configured GitHub App client.
    /// * `store` - The cache this refresher fills.
    pub(crate) fn new(
        client: GitHubAppClient,
        store: RequiredCheckStore,
    ) -> (RequiredCheckRefreshHandle, Self) {
        Self::with_source(Box::new(client), store)
    }

    /// Creates a refresher over any required-check source.
    fn with_source(
        source: Box<dyn RequiredCheckSource>,
        store: RequiredCheckStore,
    ) -> (RequiredCheckRefreshHandle, Self) {
        let (sender, requests) = mpsc::channel(REFRESH_QUEUE_CAPACITY);
        (
            RequiredCheckRefreshHandle(sender),
            Self {
                source,
                store,
                requests,
            },
        )
    }

    /// Serves refresh requests until cancellation.
    ///
    /// A request whose cache entry has already been refreshed by an earlier request is skipped
    /// without calling GitHub, so a burst of webhooks for one branch costs one API read.
    pub async fn run(mut self, mut shutdown: watch::Receiver<bool>) {
        loop {
            tokio::select! {
                biased;
                () = lifecycle::wait_for_cancellation(&mut shutdown) => return,
                request = self.requests.recv() => {
                    let Some(request) = request else {
                        return;
                    };
                    self.refresh(request).await;
                }
            }
        }
    }

    async fn refresh(&mut self, request: RequiredCheckRefreshRequest) {
        let now = OffsetDateTime::now_utc();
        match self
            .store
            .get_fresh(request.repository_id, &request.branch, now)
            .await
        {
            Ok(Some(_)) => return,
            Ok(None) => {}
            Err(error) => {
                warn!(
                    parent: None,
                    error = ?error,
                    outcome = "cache_read_failed",
                    "required-check refresh skipped"
                );
                return;
            }
        }

        let checks = match self
            .source
            .required_checks(&request.repository_name, &request.branch, now)
            .await
        {
            Ok(checks) => checks,
            Err(error) => {
                warn!(
                    parent: None,
                    error = ?error,
                    outcome = "fetch_failed",
                    "required-check refresh failed"
                );
                return;
            }
        };

        let check_count = checks.len();
        if let Err(error) = self
            .store
            .upsert(request.repository_id, &request.branch, &checks)
            .await
        {
            warn!(
                parent: None,
                error = ?error,
                outcome = "cache_write_failed",
                "required-check refresh failed"
            );
            return;
        }
        debug!(
            parent: None,
            outcome = "refreshed",
            check_count,
            "required-check cache updated"
        );
    }
}

impl std::fmt::Debug for RequiredCheckRefresher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RequiredCheckRefresher")
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{
            atomic::{AtomicUsize, Ordering},
            Arc,
        },
        time::Duration,
    };

    use async_trait::async_trait;
    use sqlx::SqlitePool;
    use time::OffsetDateTime;
    use tokio::sync::watch;

    use super::{
        RequiredCheckRefreshHandle, RequiredCheckRefreshRequest, RequiredCheckRefresher,
        RequiredCheckSource, REFRESH_QUEUE_CAPACITY,
    };
    use crate::{
        domain::repository::RepositoryId,
        github::client::GitHubClientError,
        security::CanonicalRepositoryName,
        storage::{open_database, RequiredCheckStore},
        telemetry::workflow::{DisplayName, RequiredChecks, WorkflowBranch},
    };

    const TTL: Duration = Duration::from_secs(300);

    /// A source that counts calls and answers with a fixed result.
    struct StubSource {
        calls: Arc<AtomicUsize>,
        answer: Result<Vec<&'static str>, GitHubClientError>,
    }

    impl StubSource {
        fn new(answer: Result<Vec<&'static str>, GitHubClientError>) -> (Arc<AtomicUsize>, Self) {
            let calls = Arc::new(AtomicUsize::new(0));
            (Arc::clone(&calls), Self { calls, answer })
        }
    }

    #[async_trait]
    impl RequiredCheckSource for StubSource {
        async fn required_checks(
            &mut self,
            _repository: &CanonicalRepositoryName,
            _branch: &WorkflowBranch,
            _now: OffsetDateTime,
        ) -> Result<RequiredChecks, GitHubClientError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.answer.as_ref().map_err(|error| *error).map(|names| {
                RequiredChecks::new(names.iter().filter_map(|name| DisplayName::sanitize(name)))
            })
        }
    }

    fn request(repository_id: RepositoryId) -> RequiredCheckRefreshRequest {
        RequiredCheckRefreshRequest::new(
            repository_id,
            CanonicalRepositoryName::new("Owner/Repository").expect("repository name is valid"),
            WorkflowBranch::sanitize("main").expect("branch name is valid"),
        )
    }

    async fn insert_repository(pool: &SqlitePool) -> RepositoryId {
        let id = sqlx::query_scalar(
            "INSERT INTO repositories (full_name, webhook_secret_ciphertext, \
             webhook_secret_nonce, encryption_version, enabled, created_at, updated_at) \
             VALUES ('owner/repo', X'01', X'02', 1, 1, \
             '2026-09-01T10:00:00.000Z', '2026-09-01T10:00:00.000Z') RETURNING id",
        )
        .fetch_one(pool)
        .await
        .expect("repository inserts");
        RepositoryId::new(id).expect("repository id is positive")
    }

    /// Builds a refresher over a stub source and a real store backed by a temporary database.
    async fn fixture(
        answer: Result<Vec<&'static str>, GitHubClientError>,
    ) -> (
        tempfile::TempDir,
        SqlitePool,
        RepositoryId,
        Arc<AtomicUsize>,
        RequiredCheckRefreshHandle,
        RequiredCheckRefresher,
    ) {
        let directory = tempfile::tempdir().expect("temporary directory exists");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens");
        let repository_id = insert_repository(&pool).await;
        let (calls, source) = StubSource::new(answer);
        let (handle, refresher) = RequiredCheckRefresher::with_source(
            Box::new(source),
            RequiredCheckStore::new(pool.clone(), TTL),
        );
        (directory, pool, repository_id, calls, handle, refresher)
    }

    #[tokio::test]
    async fn a_miss_fetches_once_and_caches_the_answer() {
        let (_directory, pool, repository_id, calls, _handle, mut refresher) =
            fixture(Ok(vec!["build", "test"])).await;

        refresher.refresh(request(repository_id)).await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let cached = RequiredCheckStore::new(pool, TTL)
            .get_fresh(
                repository_id,
                &WorkflowBranch::sanitize("main").expect("branch is valid"),
                OffsetDateTime::now_utc(),
            )
            .await
            .expect("cache reads")
            .expect("the refreshed entry is fresh");
        assert_eq!(cached.names().collect::<Vec<_>>(), ["build", "test"]);
    }

    #[tokio::test]
    async fn a_burst_for_one_branch_costs_exactly_one_api_read() {
        let (_directory, _pool, repository_id, calls, _handle, mut refresher) =
            fixture(Ok(vec!["build"])).await;

        for _ in 0..5 {
            refresher.refresh(request(repository_id)).await;
        }

        // The dedup skip is the property the changelog claims; the second request onward must find
        // a fresh entry and return before reaching the source at all.
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_fetch_failure_caches_nothing_and_leaves_the_branch_unknown() {
        let (_directory, pool, repository_id, calls, _handle, mut refresher) =
            fixture(Err(GitHubClientError::Forbidden)).await;

        refresher.refresh(request(repository_id)).await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(
            RequiredCheckStore::new(pool, TTL)
                .get_fresh(
                    repository_id,
                    &WorkflowBranch::sanitize("main").expect("branch is valid"),
                    OffsetDateTime::now_utc(),
                )
                .await
                .expect("cache reads")
                .is_none(),
            "a permission failure must never be cached as an answer"
        );
    }

    #[tokio::test]
    async fn a_cache_read_failure_skips_the_fetch_entirely() {
        let (_directory, pool, repository_id, calls, _handle, mut refresher) =
            fixture(Ok(vec!["build"])).await;
        // Dropping the table makes every store call fail, standing in for an unavailable database.
        sqlx::query("DROP TABLE required_check_contexts")
            .execute(&pool)
            .await
            .expect("table drops");

        refresher.refresh(request(repository_id)).await;

        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "an unreadable cache must not trigger an outbound call"
        );
    }

    #[tokio::test]
    async fn a_cache_write_failure_is_contained() {
        let (_directory, pool, repository_id, calls, _handle, mut refresher) =
            fixture(Ok(vec!["build"])).await;
        // A repository row that no longer exists fails the upsert's foreign key (the pool enables
        // `foreign_keys`), leaving the write as the only failing step after a successful read and
        // a successful fetch.
        sqlx::query("DELETE FROM repositories")
            .execute(&pool)
            .await
            .expect("repository row deletes");

        refresher.refresh(request(repository_id)).await;

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // Proves the write genuinely failed rather than the test passing vacuously.
        let cached: i64 = sqlx::query_scalar("SELECT count(*) FROM required_check_contexts")
            .fetch_one(&pool)
            .await
            .expect("cache row count reads");
        assert_eq!(cached, 0, "a failed upsert must leave no cached answer");
    }

    #[tokio::test]
    async fn a_full_queue_drops_the_newest_request_without_blocking() {
        let (_directory, _pool, repository_id, _calls, handle, _refresher) =
            fixture(Ok(vec!["build"])).await;

        // Nothing is consuming the queue, so it fills and every later request is dropped. The
        // webhook handler must never block here, so the assertion is simply that this returns.
        for _ in 0..(REFRESH_QUEUE_CAPACITY + 10) {
            handle.request(request(repository_id));
        }
    }

    #[tokio::test]
    async fn cancellation_stops_the_run_loop() {
        let (_directory, _pool, _repository_id, _calls, handle, refresher) =
            fixture(Ok(vec!["build"])).await;
        let (sender, receiver) = watch::channel(false);
        let task = tokio::spawn(refresher.run(receiver));

        sender.send_replace(true);

        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("the run loop observes cancellation")
            .expect("the run loop exits cleanly");
        drop(handle);
    }

    #[tokio::test]
    async fn dropping_every_handle_stops_the_run_loop() {
        let (_directory, _pool, _repository_id, _calls, handle, refresher) =
            fixture(Ok(vec!["build"])).await;
        let (_sender, receiver) = watch::channel(false);
        let task = tokio::spawn(refresher.run(receiver));

        drop(handle);

        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("a closed queue ends the run loop")
            .expect("the run loop exits cleanly");
    }
}
