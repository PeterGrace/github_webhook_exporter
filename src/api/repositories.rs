use axum::{
    extract::{rejection::JsonRejection, FromRequestParts, Path, State},
    http::{header, request::Parts, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use crate::{
    app::AppState,
    domain::repository::{RepositoryId, RepositoryMetadata, RepositoryMutation},
    error::AppError,
    security::{CanonicalRepositoryName, RepositorySecret},
};

pub(super) fn router() -> Router<AppState> {
    Router::new()
        .route(
            "/api/v1/repositories",
            post(create_repository).get(list_repositories),
        )
        .route(
            "/api/v1/repositories/{id}",
            get(get_repository)
                .patch(update_repository)
                .delete(delete_repository),
        )
}

struct AdminAuthorized;

impl FromRequestParts<AppState> for AdminAuthorized {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let authorization = parts
            .headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        state
            .admin_authenticator()
            .authenticate(authorization)
            .map_err(AppError::authentication)?;
        Ok(Self)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRepositoryRequest {
    full_name: String,
    webhook_secret: String,
    #[serde(default = "enabled_by_default")]
    enabled: bool,
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct PatchRepositoryRequest {
    full_name: Option<String>,
    webhook_secret: Option<String>,
    enabled: Option<bool>,
}

#[derive(Serialize)]
struct RepositoryResponse<'a> {
    id: i64,
    full_name: &'a str,
    enabled: bool,
    created_at: &'a str,
    updated_at: &'a str,
}

impl<'a> From<&'a RepositoryMetadata> for RepositoryResponse<'a> {
    fn from(metadata: &'a RepositoryMetadata) -> Self {
        Self {
            id: metadata.id().get(),
            full_name: metadata.full_name(),
            enabled: metadata.enabled(),
            created_at: metadata.created_at().as_str(),
            updated_at: metadata.updated_at().as_str(),
        }
    }
}

async fn create_repository(
    _authorized: AdminAuthorized,
    State(state): State<AppState>,
    payload: Result<Json<CreateRepositoryRequest>, JsonRejection>,
) -> Result<Response, AppError> {
    let request = payload.map_err(|_| AppError::invalid_request())?.0;
    let full_name = CanonicalRepositoryName::new(&request.full_name)
        .map_err(|_| AppError::invalid_request())?;
    let webhook_secret =
        RepositorySecret::new(request.webhook_secret).map_err(|_| AppError::invalid_request())?;
    let metadata = state
        .repository_store()
        .create(full_name, webhook_secret, request.enabled)
        .await
        .map_err(AppError::repository_store)?;

    Ok((
        StatusCode::CREATED,
        Json(RepositoryResponse::from(&metadata)),
    )
        .into_response())
}

async fn list_repositories(
    _authorized: AdminAuthorized,
    State(state): State<AppState>,
) -> Result<Response, AppError> {
    let repositories = state
        .repository_store()
        .list()
        .await
        .map_err(AppError::repository_store)?;
    let response = repositories
        .iter()
        .map(RepositoryResponse::from)
        .collect::<Vec<_>>();
    Ok(Json(response).into_response())
}

async fn get_repository(
    _authorized: AdminAuthorized,
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
) -> Result<Response, AppError> {
    let id = parse_repository_id(&raw_id)?;
    let metadata = state
        .repository_store()
        .get(id)
        .await
        .map_err(AppError::repository_store)?;
    Ok(Json(RepositoryResponse::from(&metadata)).into_response())
}

async fn update_repository(
    _authorized: AdminAuthorized,
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
    payload: Result<Json<PatchRepositoryRequest>, JsonRejection>,
) -> Result<Response, AppError> {
    let id = parse_repository_id(&raw_id)?;
    let request = payload.map_err(|_| AppError::invalid_request())?.0;
    if request.full_name.is_none() && request.webhook_secret.is_none() && request.enabled.is_none()
    {
        return Err(AppError::invalid_request());
    }

    let mut mutation = RepositoryMutation::new();
    if let Some(full_name) = request.full_name {
        mutation = mutation.with_full_name(
            CanonicalRepositoryName::new(&full_name).map_err(|_| AppError::invalid_request())?,
        );
    }
    if let Some(webhook_secret) = request.webhook_secret {
        mutation = mutation.with_webhook_secret(
            RepositorySecret::new(webhook_secret).map_err(|_| AppError::invalid_request())?,
        );
    }
    if let Some(enabled) = request.enabled {
        mutation = mutation.with_enabled(enabled);
    }

    let metadata = state
        .repository_store()
        .update(id, mutation)
        .await
        .map_err(AppError::repository_store)?;
    Ok(Json(RepositoryResponse::from(&metadata)).into_response())
}

async fn delete_repository(
    _authorized: AdminAuthorized,
    State(state): State<AppState>,
    Path(raw_id): Path<String>,
) -> Result<Response, AppError> {
    let id = parse_repository_id(&raw_id)?;
    state
        .repository_store()
        .delete(id)
        .await
        .map_err(AppError::repository_store)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

fn parse_repository_id(raw_id: &str) -> Result<RepositoryId, AppError> {
    raw_id
        .parse::<i64>()
        .ok()
        .and_then(RepositoryId::new)
        .ok_or_else(AppError::invalid_request)
}

const fn enabled_by_default() -> bool {
    true
}
