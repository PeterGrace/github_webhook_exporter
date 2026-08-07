//! Administrator-authenticated configuration and public webhook HTTP APIs.

mod merge_group;
mod pull_request;
mod repositories;
mod webhook;
mod workflow_job;

use axum::Router;

use crate::{app::AppState, metrics::Metrics};

/// Builds the repository-configuration and public GitHub webhook API router.
pub fn router(body_limit_bytes: usize, metrics: Metrics) -> Router<AppState> {
    repositories::router().merge(webhook::router(body_limit_bytes, metrics))
}
