//! Administrator-authenticated configuration and public webhook HTTP APIs.

mod ci_usage;

#[cfg(test)]
pub(crate) use ci_usage::MAX_USAGE_BODY_BYTES;
mod merge_group;
mod pull_request;
mod repositories;
mod webhook;
mod workflow_job;
mod workflow_run;

use axum::Router;

use crate::{app::AppState, metrics::Metrics};

/// Builds the repository-configuration and public GitHub webhook API router.
///
/// `POST /ci-cost/usage` is registered only when `usage_enabled`, so an unconfigured deployment
/// answers it with 404.
pub fn router(body_limit_bytes: usize, metrics: Metrics, usage_enabled: bool) -> Router<AppState> {
    let router = repositories::router().merge(webhook::router(body_limit_bytes, metrics));
    if usage_enabled {
        router.merge(ci_usage::router())
    } else {
        router
    }
}
