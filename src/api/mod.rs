//! Authenticated HTTP APIs.

mod repositories;

use axum::Router;

use crate::app::AppState;

/// Builds the versioned repository-configuration API router.
pub fn router() -> Router<AppState> {
    repositories::router()
}
