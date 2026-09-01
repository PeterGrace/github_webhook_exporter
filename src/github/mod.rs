//! Outbound GitHub App access used to answer "is this job a required status check?".
//!
//! This is the service's only outbound GitHub dependency. It never runs on the synchronous webhook
//! path: the request handler reads [`RequiredCheckStore`](crate::storage::RequiredCheckStore) and
//! nothing else, while everything here executes in a background task so a slow or unavailable
//! GitHub API can never delay, or fail, a webhook acknowledgement.

mod client;
mod refresh;

pub(crate) use client::GitHubAppClient;
pub use client::GitHubClientError;
pub use refresh::RequiredCheckRefresher;
pub(crate) use refresh::{RequiredCheckRefreshHandle, RequiredCheckRefreshRequest};
