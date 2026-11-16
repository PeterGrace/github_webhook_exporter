//! `POST /ci-cost/usage`: in-job inference and Daytona cost reports authenticated by GitHub
//! Actions OIDC.

use std::time::SystemTime;

use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, FromRequest, Request, State},
    http::{
        header::{AUTHORIZATION, CONTENT_TYPE, WWW_AUTHENTICATE},
        HeaderValue, StatusCode,
    },
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use opentelemetry::Context;
use serde::{Deserialize, Serialize};
use tracing::{error, warn};

use crate::{
    app::{AppState, RequestRepositoryContext},
    costs::CostsConfig,
    security::{
        oidc::{GitHubActionsClaims, OidcRejection, OidcToken},
        CanonicalRepositoryName,
    },
    storage::{CiUsageStoreError, RepositoryStoreError},
    telemetry::{
        usage::{log_usage_costs, UsageEntry, UsageMeasure, UsageReport, UsageSource},
        workflow::{DisplayName, WorkflowRunContext},
    },
};

/// Route of the usage endpoint.
pub(crate) const USAGE_ROUTE: &str = "/ci-cost/usage";
/// Largest accepted usage post.
pub(crate) const MAX_USAGE_BODY_BYTES: usize = 65_536;
/// Most entries one post may carry.
pub(crate) const MAX_USAGE_ENTRIES: usize = 100;
/// Largest CPU-seconds or RAM-GB-seconds one Daytona entry may report.
const MAX_ENTRY_QUANTITY: f64 = 1_000_000_000.0;
const MAX_ENTRY_USD: f64 = 10_000.0;
const SUPPORTED_VERSION: u64 = 1;

pub(super) fn router() -> Router<AppState> {
    Router::new().route(
        USAGE_ROUTE,
        post(usage_handler).layer(DefaultBodyLimit::max(MAX_USAGE_BODY_BYTES)),
    )
}

/// Why a usage post was refused: the closed `reason` vocabulary of `ci usage post rejected`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UsageRejection {
    MissingToken,
    Token(OidcRejection),
    RepositoryNotRegistered,
    RepositoryIdMismatch,
    UnsupportedMediaType,
    BodyTooLarge,
    UnsupportedVersion,
    SchemaInvalid,
    MissingUsd,
    StorageUnavailable,
}

/// A storage failure: retryable when SQLite is busy or locked, internal otherwise.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StorageFailure {
    Unavailable,
    Internal,
}

impl UsageRejection {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::MissingToken => "missing_token",
            Self::Token(rejection) => rejection.as_str(),
            Self::RepositoryNotRegistered => "repository_not_registered",
            Self::RepositoryIdMismatch => "repository_id_mismatch",
            Self::UnsupportedMediaType => "unsupported_media_type",
            Self::BodyTooLarge => "body_too_large",
            Self::UnsupportedVersion => "unsupported_version",
            Self::SchemaInvalid => "schema_invalid",
            Self::MissingUsd => "missing_usd",
            Self::StorageUnavailable => "storage_unavailable",
        }
    }

    const fn status(self) -> StatusCode {
        match self {
            Self::MissingToken
            | Self::Token(
                OidcRejection::InvalidToken
                | OidcRejection::TokenExpired
                | OidcRejection::IssuerMismatch
                | OidcRejection::AudienceMismatch,
            ) => StatusCode::UNAUTHORIZED,
            Self::Token(OidcRejection::RepositoryNotAllowed)
            | Self::RepositoryNotRegistered
            | Self::RepositoryIdMismatch => StatusCode::FORBIDDEN,
            Self::Token(OidcRejection::JwksUnavailable) | Self::StorageUnavailable => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::UnsupportedMediaType => StatusCode::UNSUPPORTED_MEDIA_TYPE,
            Self::BodyTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            Self::SchemaInvalid => StatusCode::BAD_REQUEST,
            Self::UnsupportedVersion | Self::MissingUsd => StatusCode::UNPROCESSABLE_ENTITY,
        }
    }
}

#[derive(Serialize)]
struct RejectionBody {
    code: &'static str,
    message: &'static str,
}

/// Answers a storage failure. A busy or locked database is a retryable
/// `503 storage_unavailable`. Anything else will fail the same way on retry, so it is a
/// `500 internal_error` whose `error_id` matches one local error log.
fn storage_failure(failure: StorageFailure, repository: &CanonicalRepositoryName) -> Response {
    if failure == StorageFailure::Unavailable {
        return reject(UsageRejection::StorageUnavailable, Some(repository));
    }
    let error_id = crate::error::ErrorCorrelationId::new();
    let _parentless_context = Context::new().attach();
    error!(
        parent: None,
        outcome = "internal_error",
        %error_id,
        github.repository = repository.as_str(),
        "ci usage post failed"
    );
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(InternalErrorBody {
            code: "internal_error",
            message: "internal server error",
            error_id,
        }),
    )
        .into_response()
}

#[derive(Serialize)]
struct InternalErrorBody {
    code: &'static str,
    message: &'static str,
    error_id: crate::error::ErrorCorrelationId,
}

/// Logs the rejection and builds its response. The token and body never reach either.
fn reject(rejection: UsageRejection, repository: Option<&CanonicalRepositoryName>) -> Response {
    let _parentless_context = Context::new().attach();
    warn!(
        parent: None,
        reason = rejection.as_str(),
        github.repository = repository.map(CanonicalRepositoryName::as_str),
        "ci usage post rejected"
    );
    let status = rejection.status();
    let mut response = (
        status,
        Json(RejectionBody {
            code: rejection.as_str(),
            message: "usage report rejected",
        }),
    )
        .into_response();
    if status == StatusCode::UNAUTHORIZED {
        response
            .headers_mut()
            .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    response
}

#[derive(Deserialize)]
struct VersionProbe {
    version: Option<serde_json::Value>,
}

/// The v1 wire body. Attribution fields such as `repository` are unknown fields and fail.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UsagePostV1 {
    #[allow(dead_code, reason = "read by VersionProbe before this parse")]
    version: u64,
    #[serde(default)]
    job: Option<String>,
    entries: Vec<RawEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawEntry {
    source: String,
    #[serde(default)]
    usd: Option<f64>,
    #[serde(default)]
    origin: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    task: Option<String>,
    #[serde(default)]
    input_tokens: Option<u32>,
    #[serde(default)]
    output_tokens: Option<u32>,
    #[serde(default)]
    cpu_seconds: Option<f64>,
    #[serde(default)]
    ram_gb_seconds: Option<f64>,
}

/// A validated v1 post, before attribution is attached.
struct ParsedPost {
    job: Option<DisplayName>,
    entries: Vec<UsageEntry>,
}

fn bounded(value: f64, max: f64) -> Option<f64> {
    (value.is_finite() && (0.0..=max).contains(&value)).then_some(value + 0.0)
}

fn label(value: Option<&str>) -> Result<Option<DisplayName>, UsageRejection> {
    value
        .map(|value| DisplayName::sanitize(value).ok_or(UsageRejection::SchemaInvalid))
        .transpose()
}

/// Parses and prices a post body.
///
/// Daytona entries are priced from `costs.daytona`. Without those rates they are recorded at $0
/// with `fell_back` set, so a missing rate never fails a CI job and the seconds stay stored.
fn parse_post(body: &[u8], costs: Option<&CostsConfig>) -> Result<ParsedPost, UsageRejection> {
    let probe: VersionProbe =
        serde_json::from_slice(body).map_err(|_| UsageRejection::SchemaInvalid)?;
    match probe.version.as_ref().and_then(serde_json::Value::as_u64) {
        Some(SUPPORTED_VERSION) => {}
        Some(_) => return Err(UsageRejection::UnsupportedVersion),
        None if probe.version.is_some() => return Err(UsageRejection::UnsupportedVersion),
        None => return Err(UsageRejection::SchemaInvalid),
    }
    let post: UsagePostV1 =
        serde_json::from_slice(body).map_err(|_| UsageRejection::SchemaInvalid)?;
    if post.entries.is_empty() || post.entries.len() > MAX_USAGE_ENTRIES {
        return Err(UsageRejection::SchemaInvalid);
    }
    let entries = post
        .entries
        .into_iter()
        .map(|entry| price_entry(entry, costs))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ParsedPost {
        job: label(post.job.as_deref())?,
        entries,
    })
}

fn price_entry(entry: RawEntry, costs: Option<&CostsConfig>) -> Result<UsageEntry, UsageRejection> {
    let source = UsageSource::parse(&entry.source).ok_or(UsageRejection::SchemaInvalid)?;
    if source.as_str() == crate::telemetry::trace::CI_SOURCE_GITHUB_ACTIONS {
        return Err(UsageRejection::SchemaInvalid);
    }
    let task = label(entry.task.as_deref())?;
    if source.is_daytona() {
        if entry.usd.is_some()
            || entry.model.is_some()
            || entry.origin.is_some()
            || entry.input_tokens.is_some()
            || entry.output_tokens.is_some()
        {
            return Err(UsageRejection::SchemaInvalid);
        }
        let cpu_seconds = entry
            .cpu_seconds
            .and_then(|value| bounded(value, MAX_ENTRY_QUANTITY))
            .ok_or(UsageRejection::SchemaInvalid)?;
        let ram_gb_seconds = entry
            .ram_gb_seconds
            .and_then(|value| bounded(value, MAX_ENTRY_QUANTITY))
            .ok_or(UsageRejection::SchemaInvalid)?;
        let rates = costs.and_then(CostsConfig::daytona_rates);
        let cost_usd = rates.map_or(0.0, |rates| {
            cpu_seconds * rates.usd_per_cpu_second.usd()
                + ram_gb_seconds * rates.usd_per_ram_gb_second.usd()
        });
        return Ok(UsageEntry {
            source,
            task,
            model: None,
            origin: None,
            cost_usd,
            fell_back: rates.is_none(),
            price_table_version: costs.map(|costs| costs.version().to_owned()),
            measure: UsageMeasure::Sandbox {
                cpu_seconds,
                ram_gb_seconds,
            },
        });
    }
    if entry.cpu_seconds.is_some() || entry.ram_gb_seconds.is_some() {
        return Err(UsageRejection::SchemaInvalid);
    }
    let cost_usd = entry.usd.ok_or(UsageRejection::MissingUsd)?;
    let cost_usd = bounded(cost_usd, MAX_ENTRY_USD).ok_or(UsageRejection::SchemaInvalid)?;
    Ok(UsageEntry {
        source,
        task,
        model: label(entry.model.as_deref())?,
        origin: label(entry.origin.as_deref())?,
        cost_usd,
        fell_back: false,
        price_table_version: None,
        measure: UsageMeasure::Inference {
            input_tokens: entry.input_tokens.map(i64::from),
            output_tokens: entry.output_tokens.map(i64::from),
        },
    })
}

async fn usage_handler(State(state): State<AppState>, request: Request) -> Response {
    let received_at = SystemTime::now();
    let Some(verifier) = state.oidc_verifier() else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let (parts, body) = request.into_parts();
    let Some(token) = parts
        .headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(OidcToken::from_authorization)
    else {
        return reject(UsageRejection::MissingToken, None);
    };
    let claims = match verifier.verify(&token).await {
        Ok(claims) => claims,
        Err(rejection) => return reject(UsageRejection::Token(rejection), None),
    };
    drop(token);
    if let Some(context) = parts.extensions.get::<RequestRepositoryContext>() {
        context.authenticate(&claims.repository);
    }
    let repository = Some(&claims.repository);
    let repository_id = match state
        .repository_store()
        .enabled_id(&claims.repository)
        .await
    {
        Ok(Some(repository_id)) => repository_id,
        Ok(None) => return reject(UsageRejection::RepositoryNotRegistered, repository),
        Err(RepositoryStoreError::Unavailable) => {
            return storage_failure(StorageFailure::Unavailable, &claims.repository);
        }
        Err(_) => return storage_failure(StorageFailure::Internal, &claims.repository),
    };
    match state
        .ci_usage_store()
        .pin_repository(repository_id, claims.github_repository_id)
        .await
    {
        Ok(true) => {}
        Ok(false) => return reject(UsageRejection::RepositoryIdMismatch, repository),
        Err(CiUsageStoreError::Unavailable) => {
            return storage_failure(StorageFailure::Unavailable, &claims.repository);
        }
        Err(CiUsageStoreError::Internal) => {
            return storage_failure(StorageFailure::Internal, &claims.repository);
        }
    }

    let is_json = parts
        .headers
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .is_some_and(|media_type| media_type.trim().eq_ignore_ascii_case("application/json"));
    if !is_json {
        return reject(UsageRejection::UnsupportedMediaType, repository);
    }
    let body = match Bytes::from_request(Request::from_parts(parts, body), &state).await {
        Ok(body) => body,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return reject(UsageRejection::BodyTooLarge, repository);
        }
        Err(_) => return reject(UsageRejection::SchemaInvalid, repository),
    };
    let post = match parse_post(&body, state.costs()) {
        Ok(post) => post,
        Err(rejection) => return reject(rejection, repository),
    };

    let pull_request = attributed_pull_request(&state, repository_id, &claims).await;
    let report = UsageReport {
        claims,
        job: post.job,
        pull_request,
        received_at,
        entries: post.entries,
    };
    if let Err(failure) = state.ci_usage_store().record(repository_id, &report).await {
        let failure = match failure {
            CiUsageStoreError::Unavailable => StorageFailure::Unavailable,
            CiUsageStoreError::Internal => StorageFailure::Internal,
        };
        return storage_failure(failure, &report.claims.repository);
    }
    log_usage_costs(&report);
    state.workflow_trace_emitter().emit_usage(&report);
    StatusCode::NO_CONTENT.into_response()
}

/// Reads the run's attributed PR from the stored `workflow_run` context.
///
/// The context usually arrives before any job step runs. When it has not, or the lookup fails,
/// the entry is recorded without a PR; run totals can still join it by run ID.
async fn attributed_pull_request(
    state: &AppState,
    repository_id: crate::domain::repository::RepositoryId,
    claims: &GitHubActionsClaims,
) -> Option<crate::domain::merge_queue::PullRequestNumber> {
    state
        .workflow_run_store()
        .get(repository_id, claims.run_id, claims.run_attempt)
        .await
        .ok()
        .flatten()
        .as_ref()
        .and_then(WorkflowRunContext::pull_request_ref)
        .copied()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{parse_post, UsageRejection, MAX_USAGE_ENTRIES};
    use crate::{costs::CostsConfig, telemetry::usage::UsageMeasure};

    fn costs(with_daytona: bool) -> CostsConfig {
        let daytona = if with_daytona {
            r#", "daytona": {"usd_per_cpu_second": 0.00002, "usd_per_ram_gb_second": 0.000005}"#
        } else {
            ""
        };
        CostsConfig::from_json(&format!(
            r#"{{"runners": {{"default_usd_per_minute": 0.006}}{daytona}}}"#
        ))
        .expect("costs are valid")
    }

    fn parse(
        body: serde_json::Value,
        costs: Option<&CostsConfig>,
    ) -> Result<Vec<f64>, UsageRejection> {
        parse_post(body.to_string().as_bytes(), costs)
            .map(|post| post.entries.iter().map(|entry| entry.cost_usd).collect())
    }

    #[test]
    fn inference_keeps_the_callers_usd_and_daytona_is_priced_from_the_table() {
        let table = costs(true);
        let post = parse_post(
            json!({"version": 1, "job": "run-evals", "entries": [
                {"source": "anthropic", "origin": "custom", "model": "claude-sonnet-5-5",
                 "task": "grade", "usd": 0.381, "input_tokens": 88400, "output_tokens": 9100},
                {"source": "daytona", "task": "sandbox", "cpu_seconds": 1840,
                 "ram_gb_seconds": 7360}
            ]})
            .to_string()
            .as_bytes(),
            Some(&table),
        )
        .expect("post is valid");

        assert_eq!(post.job.as_ref().map(|job| job.as_str()), Some("run-evals"));
        let [inference, sandbox] = post.entries.as_slice() else {
            panic!("two entries");
        };
        assert_eq!(inference.cost_usd, 0.381);
        assert!(!inference.fell_back);
        assert_eq!(inference.price_table_version, None);
        assert_eq!(
            inference.measure,
            UsageMeasure::Inference {
                input_tokens: Some(88_400),
                output_tokens: Some(9_100)
            }
        );
        assert_eq!(sandbox.cost_usd, 1_840.0 * 0.00002 + 7_360.0 * 0.000005);
        assert!(!sandbox.fell_back);
        assert_eq!(
            sandbox.price_table_version.as_deref(),
            Some(table.version())
        );
    }

    #[test]
    fn daytona_without_rates_is_recorded_at_zero_and_flagged() {
        for table in [Some(costs(false)), None] {
            let post = parse_post(
                json!({"version": 1, "entries": [
                    {"source": "daytona", "cpu_seconds": 10, "ram_gb_seconds": 20}
                ]})
                .to_string()
                .as_bytes(),
                table.as_ref(),
            )
            .expect("post is valid");
            let entry = &post.entries[0];
            assert_eq!(entry.cost_usd, 0.0);
            assert!(entry.fell_back);
            assert_eq!(
                entry.price_table_version.as_deref(),
                table.as_ref().map(CostsConfig::version)
            );
        }
    }

    #[test]
    fn each_body_failure_maps_to_its_reason() {
        let too_many = (0..=MAX_USAGE_ENTRIES)
            .map(|_| json!({"source": "anthropic", "usd": 0.1}))
            .collect::<Vec<_>>();
        for (body, expected) in [
            (
                json!({"version": 2, "entries": []}),
                UsageRejection::UnsupportedVersion,
            ),
            (
                json!({"version": "1", "entries": []}),
                UsageRejection::UnsupportedVersion,
            ),
            (json!({"entries": []}), UsageRejection::SchemaInvalid),
            (
                json!({"version": 1, "entries": [{"source": "anthropic"}]}),
                UsageRejection::MissingUsd,
            ),
            (
                json!({"version": 1, "repository": "o/r",
                       "entries": [{"source": "anthropic", "usd": 1}]}),
                UsageRejection::SchemaInvalid,
            ),
            (
                json!({"version": 1, "entries": [{"source": "anthropic", "usd": 1,
                                                   "run_id": 9}]}),
                UsageRejection::SchemaInvalid,
            ),
            (
                json!({"version": 1, "entries": []}),
                UsageRejection::SchemaInvalid,
            ),
            (
                json!({"version": 1, "entries": too_many}),
                UsageRejection::SchemaInvalid,
            ),
            (
                json!({"version": 1, "entries": [{"source": "anthropic", "usd": -0.5}]}),
                UsageRejection::SchemaInvalid,
            ),
            (
                json!({"version": 1, "entries": [{"source": "anthropic", "usd": 1e9}]}),
                UsageRejection::SchemaInvalid,
            ),
            (
                json!({"version": 1, "entries": [{"source": "Anthropic", "usd": 1}]}),
                UsageRejection::SchemaInvalid,
            ),
            (
                json!({"version": 1, "entries": [{"source": "github_actions", "usd": 1}]}),
                UsageRejection::SchemaInvalid,
            ),
            (
                json!({"version": 1, "entries": [{"source": "daytona", "cpu_seconds": 1}]}),
                UsageRejection::SchemaInvalid,
            ),
            (
                json!({"version": 1, "entries": [{"source": "daytona", "cpu_seconds": 1,
                                                   "ram_gb_seconds": 1, "usd": 3}]}),
                UsageRejection::SchemaInvalid,
            ),
            (
                json!({"version": 1, "entries": [{"source": "anthropic", "usd": 1,
                                                   "cpu_seconds": 1}]}),
                UsageRejection::SchemaInvalid,
            ),
        ] {
            assert_eq!(parse(body.clone(), None), Err(expected), "{body}");
        }
        assert_eq!(
            parse_post(b"{not json", None).map(|_| ()).err(),
            Some(UsageRejection::SchemaInvalid)
        );
    }
}
