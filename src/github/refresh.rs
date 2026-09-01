use time::OffsetDateTime;
use tokio::sync::{mpsc, watch};
use tracing::{debug, warn};

use crate::{
    domain::repository::RepositoryId, lifecycle, security::CanonicalRepositoryName,
    storage::RequiredCheckStore, telemetry::workflow::WorkflowBranch,
};

use super::client::GitHubAppClient;

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
            debug!(
                outcome = "queue_full",
                "required-check refresh request dropped"
            );
        }
    }
}

/// The background task that fills the required-check cache from the GitHub API.
///
/// Owning the only [`GitHubAppClient`] keeps the installation token in exactly one place, so a
/// burst of refreshes shares one minted token.
pub struct RequiredCheckRefresher {
    client: GitHubAppClient,
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
        let (sender, requests) = mpsc::channel(REFRESH_QUEUE_CAPACITY);
        (
            RequiredCheckRefreshHandle(sender),
            Self {
                client,
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
            .client
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
            .field("client", &self.client)
            .finish_non_exhaustive()
    }
}
