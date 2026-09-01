use std::{fmt, time::Duration};

use jsonwebtoken::{Algorithm, EncodingKey, Header};
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use secrecy::zeroize::Zeroizing;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use time::OffsetDateTime;

use crate::{
    config::GitHubAppConfig,
    security::CanonicalRepositoryName,
    telemetry::workflow::{DisplayName, RequiredChecks, WorkflowBranch, MAX_REQUIRED_CHECK_COUNT},
};

/// Maximum accepted branch-protection response body.
///
/// The response is a short list of check names; anything larger is a misconfigured endpoint rather
/// than a payload worth buffering.
const MAX_RESPONSE_BYTES: usize = 262_144;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Lifetime of the app JWT. GitHub rejects anything above ten minutes.
const APP_JWT_LIFETIME_SECONDS: i64 = 540;
/// Backdating absorbs clock skew between this process and GitHub.
const APP_JWT_BACKDATE_SECONDS: i64 = 60;
/// Renew an installation token this long before it actually expires.
const INSTALLATION_TOKEN_RENEWAL_SKEW: time::Duration = time::Duration::seconds(60);
const USER_AGENT: &str = concat!("github-webhook-exporter/", env!("CARGO_PKG_VERSION"));
const GITHUB_API_VERSION: &str = "2022-11-28";
const GITHUB_ACCEPT: &str = "application/vnd.github+json";
/// Percent-encodes everything a branch name may contain that would otherwise change the request
/// target. `/` is deliberately left literal: GitHub resolves multi-segment branch names such as
/// `gh-readonly-queue/main/pr-7` only when the separators stay unencoded.
const BRANCH_SEGMENT_ENCODE_SET: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'%')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}');

/// A GitHub App client that reads branch-protection required status checks.
///
/// One installation token is minted on demand and reused until shortly before it expires, so a
/// burst of refreshes costs one token mint rather than one per branch.
pub(crate) struct GitHubAppClient {
    http: reqwest::Client,
    encoding_key: EncodingKey,
    app_id: u64,
    installation_id: u64,
    api_base_url: String,
    installation_token: Option<CachedInstallationToken>,
}

impl GitHubAppClient {
    /// Creates a client from validated GitHub App configuration.
    ///
    /// # Parameters
    ///
    /// * `config` - The validated App identifiers, PEM private key, and API base URL.
    ///
    /// # Errors
    ///
    /// Returns [`GitHubClientError::PrivateKey`] when the configured PEM is not a usable RSA
    /// private key, and [`GitHubClientError::Transport`] when the HTTP client cannot be built.
    pub(crate) fn new(config: &GitHubAppConfig) -> Result<Self, GitHubClientError> {
        let encoding_key = EncodingKey::from_rsa_pem(config.private_key_pem())
            .map_err(|_| GitHubClientError::PrivateKey)?;
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(USER_AGENT)
            .build()
            .map_err(|_| GitHubClientError::Transport)?;
        Ok(Self {
            http,
            encoding_key,
            app_id: config.app_id(),
            installation_id: config.installation_id(),
            api_base_url: config.api_base_url().trim_end_matches('/').to_owned(),
            installation_token: None,
        })
    }

    /// Reads the branch-protection required status checks of one repository branch.
    ///
    /// # Parameters
    ///
    /// * `repository` - The canonical authenticated repository name.
    /// * `branch` - The workflow run's target branch.
    /// * `now` - The instant used to judge installation-token expiry.
    ///
    /// # Returns
    ///
    /// The bounded set of required check names. An unprotected branch answers `404`, which is a
    /// confident empty set: nothing is required there.
    ///
    /// # Errors
    ///
    /// Returns [`GitHubClientError`] when the token cannot be minted, the request fails, GitHub
    /// answers with an unexpected status, or the response body cannot be decoded. Every variant is
    /// bounded and carries no response content.
    pub(crate) async fn required_checks(
        &mut self,
        repository: &CanonicalRepositoryName,
        branch: &WorkflowBranch,
        now: OffsetDateTime,
    ) -> Result<RequiredChecks, GitHubClientError> {
        let token = self.installation_token(now).await?;
        let encoded_branch = utf8_percent_encode(branch.as_str(), BRANCH_SEGMENT_ENCODE_SET);
        let url = format!(
            "{}/repos/{}/branches/{encoded_branch}/protection/required_status_checks",
            self.api_base_url,
            repository.as_str(),
        );
        let response = self
            .http
            .get(url)
            .header(reqwest::header::ACCEPT, GITHUB_ACCEPT)
            .header("x-github-api-version", GITHUB_API_VERSION)
            .bearer_auth(token.as_str())
            .send()
            .await
            .map_err(|_| GitHubClientError::Transport)?;

        // `404` is the documented answer for "branch not protected", which is a real answer rather
        // than a failure: an unprotected branch requires no checks at all.
        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(RequiredChecks::default());
        }
        let body = read_bounded_body(response).await?;
        let projection: RequiredStatusChecksProjection =
            serde_json::from_slice(&body).map_err(|_| GitHubClientError::Decode)?;
        Ok(projection.into_required_checks())
    }

    /// Returns a live installation token, minting a new one when the cached token is near expiry.
    async fn installation_token(
        &mut self,
        now: OffsetDateTime,
    ) -> Result<Zeroizing<String>, GitHubClientError> {
        if let Some(cached) = &self.installation_token {
            if cached.is_usable_at(now) {
                return Ok(cached.token.clone());
            }
        }
        let minted = self.mint_installation_token(now).await?;
        let token = minted.token.clone();
        self.installation_token = Some(minted);
        Ok(token)
    }

    async fn mint_installation_token(
        &self,
        now: OffsetDateTime,
    ) -> Result<CachedInstallationToken, GitHubClientError> {
        let jwt = self.app_jwt(now)?;
        let url = format!(
            "{}/app/installations/{}/access_tokens",
            self.api_base_url, self.installation_id
        );
        let response = self
            .http
            .post(url)
            .header(reqwest::header::ACCEPT, GITHUB_ACCEPT)
            .header("x-github-api-version", GITHUB_API_VERSION)
            .header(reqwest::header::CONTENT_LENGTH, 0)
            .bearer_auth(jwt.as_str())
            .send()
            .await
            .map_err(|_| GitHubClientError::Transport)?;
        let body = read_bounded_body(response).await?;
        let projection: InstallationTokenProjection =
            serde_json::from_slice(&body).map_err(|_| GitHubClientError::Decode)?;
        projection.into_cached_token()
    }

    /// Signs the short-lived RS256 JWT that authenticates this process as the GitHub App itself.
    fn app_jwt(&self, now: OffsetDateTime) -> Result<Zeroizing<String>, GitHubClientError> {
        let issued_at = now.unix_timestamp() - APP_JWT_BACKDATE_SECONDS;
        let claims = AppJwtClaims {
            issued_at,
            expires_at: issued_at + APP_JWT_LIFETIME_SECONDS,
            issuer: self.app_id.to_string(),
        };
        jsonwebtoken::encode(&Header::new(Algorithm::RS256), &claims, &self.encoding_key)
            .map(Zeroizing::new)
            .map_err(|_| GitHubClientError::Jwt)
    }
}

impl fmt::Debug for GitHubAppClient {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubAppClient")
            .field("app_id", &self.app_id)
            .field("installation_id", &self.installation_id)
            .field("api_base_url", &self.api_base_url)
            .field("private_key", &"[REDACTED]")
            .field("installation_token", &"[REDACTED]")
            .finish()
    }
}

/// Reads at most [`MAX_RESPONSE_BYTES`] after rejecting a non-success status.
async fn read_bounded_body(response: reqwest::Response) -> Result<Vec<u8>, GitHubClientError> {
    let status = response.status();
    if !status.is_success() {
        return Err(GitHubClientError::from_status(status));
    }
    let body = response
        .bytes()
        .await
        .map_err(|_| GitHubClientError::Transport)?;
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(GitHubClientError::Decode);
    }
    Ok(body.to_vec())
}

struct CachedInstallationToken {
    token: Zeroizing<String>,
    expires_at: OffsetDateTime,
}

impl fmt::Debug for CachedInstallationToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CachedInstallationToken")
            .field("token", &"[REDACTED]")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl CachedInstallationToken {
    /// Returns whether this token is still usable, keeping a renewal margin before expiry.
    fn is_usable_at(&self, now: OffsetDateTime) -> bool {
        now + INSTALLATION_TOKEN_RENEWAL_SKEW < self.expires_at
    }
}

#[derive(Serialize)]
struct AppJwtClaims {
    #[serde(rename = "iat")]
    issued_at: i64,
    #[serde(rename = "exp")]
    expires_at: i64,
    #[serde(rename = "iss")]
    issuer: String,
}

#[derive(Deserialize)]
struct InstallationTokenProjection {
    token: String,
    expires_at: String,
}

impl InstallationTokenProjection {
    fn into_cached_token(self) -> Result<CachedInstallationToken, GitHubClientError> {
        let expires_at = OffsetDateTime::parse(
            &self.expires_at,
            &time::format_description::well_known::Rfc3339,
        )
        .map_err(|_| GitHubClientError::Decode)?;
        if self.token.is_empty() {
            return Err(GitHubClientError::Decode);
        }
        Ok(CachedInstallationToken {
            token: Zeroizing::new(self.token),
            expires_at,
        })
    }
}

/// The bounded projection of a branch-protection required-status-checks response.
///
/// GitHub reports the same names twice: `checks` is the current representation and `contexts` the
/// legacy one. `checks` wins when both are present.
#[derive(Deserialize)]
struct RequiredStatusChecksProjection {
    #[serde(default)]
    contexts: Vec<String>,
    #[serde(default)]
    checks: Vec<RequiredStatusCheckProjection>,
}

#[derive(Deserialize)]
struct RequiredStatusCheckProjection {
    context: String,
}

impl RequiredStatusChecksProjection {
    fn into_required_checks(self) -> RequiredChecks {
        if self.checks.is_empty() {
            RequiredChecks::new(
                self.contexts
                    .iter()
                    .filter_map(|context| DisplayName::sanitize(context))
                    .take(MAX_REQUIRED_CHECK_COUNT),
            )
        } else {
            RequiredChecks::new(
                self.checks
                    .iter()
                    .filter_map(|check| DisplayName::sanitize(&check.context))
                    .take(MAX_REQUIRED_CHECK_COUNT),
            )
        }
    }
}

/// A stable, redacted outbound GitHub failure.
///
/// No variant carries a response body, a URL, or credential material, so these errors are safe to
/// log verbatim.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
pub enum GitHubClientError {
    /// The configured private key is not a usable RSA PEM key.
    #[error("GitHub App private key is unusable")]
    PrivateKey,
    /// The app JWT could not be signed.
    #[error("failed to sign the GitHub App JWT")]
    Jwt,
    /// The HTTP client could not be built, or the request never completed.
    #[error("GitHub API request failed")]
    Transport,
    /// GitHub rejected the credential.
    #[error("GitHub API rejected the App credential")]
    Unauthorized,
    /// The installation lacks the permission this read needs.
    #[error("GitHub App installation lacks branch-protection read permission")]
    Forbidden,
    /// GitHub applied a rate limit or secondary rate limit.
    #[error("GitHub API rate limit reached")]
    RateLimited,
    /// GitHub answered with another unexpected status.
    #[error("GitHub API returned an unexpected status")]
    UnexpectedStatus,
    /// The response body was too large or did not match the expected shape.
    #[error("GitHub API response could not be decoded")]
    Decode,
}

impl GitHubClientError {
    fn from_status(status: reqwest::StatusCode) -> Self {
        match status {
            reqwest::StatusCode::UNAUTHORIZED => Self::Unauthorized,
            // GitHub answers a spent rate limit with `403` or `429`; both mean "retry later"
            // rather than "this installation may not read branch protection".
            reqwest::StatusCode::TOO_MANY_REQUESTS => Self::RateLimited,
            reqwest::StatusCode::FORBIDDEN => Self::Forbidden,
            _ => Self::UnexpectedStatus,
        }
    }
}

#[cfg(test)]
impl GitHubAppClient {
    /// Creates a client with a pre-installed installation token.
    ///
    /// Tests exercise request construction and response handling; minting a token would require a
    /// real RSA private key, and this project keeps private keys out of the repository entirely.
    fn with_cached_token(api_base_url: &str, expires_at: OffsetDateTime) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(REQUEST_TIMEOUT)
                .build()
                .expect("test client builds"),
            encoding_key: EncodingKey::from_secret(b"unused-in-tests"),
            app_id: 1,
            installation_id: 2,
            api_base_url: api_base_url.trim_end_matches('/').to_owned(),
            installation_token: Some(CachedInstallationToken {
                token: Zeroizing::new("installation-token".to_owned()),
                expires_at,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use secrecy::zeroize::Zeroizing;
    use time::OffsetDateTime;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    use super::{
        CachedInstallationToken, GitHubAppClient, GitHubClientError, InstallationTokenProjection,
        RequiredStatusCheckProjection, RequiredStatusChecksProjection,
    };
    use crate::{
        security::CanonicalRepositoryName,
        telemetry::workflow::{WorkflowBranch, MAX_REQUIRED_CHECK_COUNT},
    };

    /// Runs one `required_checks` call against a canned response.
    async fn required_checks_against(
        status_line: &'static str,
        body: &'static str,
        branch: &str,
    ) -> (Result<Vec<String>, GitHubClientError>, String) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener binds");
        let address = listener
            .local_addr()
            .expect("listener address is available");
        let captured = Arc::new(Mutex::new(String::new()));
        let server_captured = Arc::clone(&captured);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("request arrives");
            let mut request = [0_u8; 2048];
            let read = stream.read(&mut request).await.expect("request reads");
            *server_captured.lock().expect("capture lock is available") =
                String::from_utf8_lossy(&request[..read]).into_owned();
            let response = format!(
                "{status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        });

        let now = OffsetDateTime::now_utc();
        let mut client = GitHubAppClient::with_cached_token(
            &format!("http://{address}"),
            now + time::Duration::hours(1),
        );
        let result = client
            .required_checks(
                &CanonicalRepositoryName::new("Owner/Repository").expect("name is valid"),
                &WorkflowBranch::sanitize(branch).expect("branch is valid"),
                now,
            )
            .await
            .map(|checks| checks.names().map(str::to_owned).collect::<Vec<_>>());
        server.await.expect("server task completes");
        let request = captured.lock().expect("capture lock is available").clone();
        (result, request)
    }

    #[tokio::test]
    async fn a_protected_branch_reports_its_required_check_names() {
        let (result, request) = required_checks_against(
            "HTTP/1.1 200 OK",
            r#"{"strict":true,"contexts":["legacy"],"checks":[{"context":"build","app_id":1},{"context":"test","app_id":1}]}"#,
            "main",
        )
        .await;

        assert_eq!(result.expect("response parses"), ["build", "test"]);
        assert!(
            request.starts_with(
                "GET /repos/owner/repository/branches/main/protection/required_status_checks "
            ),
            "unexpected request line in:\n{request}"
        );
        assert!(request.contains("authorization: Bearer installation-token"));
        assert!(request.contains("x-github-api-version: 2022-11-28"));
    }

    #[tokio::test]
    async fn the_legacy_contexts_list_is_used_only_when_checks_is_empty() {
        let (result, _) = required_checks_against(
            "HTTP/1.1 200 OK",
            r#"{"contexts":["legacy-one","legacy-two"],"checks":[]}"#,
            "main",
        )
        .await;

        assert_eq!(
            result.expect("response parses"),
            ["legacy-one", "legacy-two"]
        );
    }

    #[tokio::test]
    async fn an_unprotected_branch_is_a_confident_empty_answer_rather_than_a_failure() {
        let (result, _) = required_checks_against(
            "HTTP/1.1 404 Not Found",
            r#"{"message":"Branch not protected"}"#,
            "main",
        )
        .await;

        assert_eq!(result.expect("404 is an answer"), Vec::<String>::new());
    }

    #[tokio::test]
    async fn a_multi_segment_branch_keeps_its_separators_unencoded() {
        let (_, request) = required_checks_against(
            "HTTP/1.1 200 OK",
            r#"{"contexts":[],"checks":[]}"#,
            "gh-readonly-queue/main/pr 7",
        )
        .await;

        assert!(
            request.starts_with(
                "GET /repos/owner/repository/branches/gh-readonly-queue/main/pr%207/protection/required_status_checks "
            ),
            "unexpected request line in:\n{request}"
        );
    }

    #[tokio::test]
    async fn rejected_credentials_and_permissions_map_to_distinct_bounded_errors() {
        for (status_line, expected) in [
            ("HTTP/1.1 401 Unauthorized", GitHubClientError::Unauthorized),
            ("HTTP/1.1 403 Forbidden", GitHubClientError::Forbidden),
            (
                "HTTP/1.1 429 Too Many Requests",
                GitHubClientError::RateLimited,
            ),
            (
                "HTTP/1.1 500 Internal Server Error",
                GitHubClientError::UnexpectedStatus,
            ),
        ] {
            let (result, _) =
                required_checks_against(status_line, r#"{"message":"nope"}"#, "main").await;

            assert_eq!(
                result.expect_err("failure status is an error"),
                expected,
                "unexpected mapping for {status_line}"
            );
        }
    }

    #[tokio::test]
    async fn a_malformed_success_body_is_a_decode_failure() {
        let (result, _) =
            required_checks_against("HTTP/1.1 200 OK", "definitely not json", "main").await;

        assert_eq!(
            result.expect_err("malformed body is an error"),
            GitHubClientError::Decode
        );
    }

    #[tokio::test]
    async fn an_expiring_installation_token_is_reminted_before_it_lapses() {
        let now = OffsetDateTime::now_utc();
        // The mint endpoint is unreachable, so a remint attempt surfaces as a transport failure.
        // That failure is itself the assertion: a usable token would have skipped the mint.
        let mut client = GitHubAppClient::with_cached_token(
            "http://127.0.0.1:1",
            now + time::Duration::seconds(30),
        );

        let error = client
            .installation_token(now)
            .await
            .expect_err("token is reminted");

        assert!(matches!(
            error,
            GitHubClientError::Jwt | GitHubClientError::Transport
        ));
    }

    #[test]
    fn a_cached_token_stays_usable_only_outside_the_renewal_margin() {
        let now = OffsetDateTime::now_utc();
        let token = |seconds| CachedInstallationToken {
            token: Zeroizing::new("token".to_owned()),
            expires_at: now + time::Duration::seconds(seconds),
        };

        assert!(token(3_600).is_usable_at(now));
        assert!(!token(60).is_usable_at(now));
        assert!(!token(-1).is_usable_at(now));
    }

    #[test]
    fn an_installation_token_response_must_carry_a_token_and_a_parsable_expiry() {
        assert!(InstallationTokenProjection {
            token: "ghs_value".to_owned(),
            expires_at: "2026-09-01T10:00:00Z".to_owned(),
        }
        .into_cached_token()
        .is_ok());
        assert_eq!(
            InstallationTokenProjection {
                token: String::new(),
                expires_at: "2026-09-01T10:00:00Z".to_owned(),
            }
            .into_cached_token()
            .expect_err("empty token is rejected"),
            GitHubClientError::Decode
        );
        assert_eq!(
            InstallationTokenProjection {
                token: "ghs_value".to_owned(),
                expires_at: "not a timestamp".to_owned(),
            }
            .into_cached_token()
            .expect_err("unparsable expiry is rejected"),
            GitHubClientError::Decode
        );
    }

    #[test]
    fn required_check_projections_sanitize_names_and_stay_bounded() {
        let projection = RequiredStatusChecksProjection {
            contexts: Vec::new(),
            checks: (0..150)
                .map(|index| RequiredStatusCheckProjection {
                    context: format!("check-{index:03}"),
                })
                .chain([RequiredStatusCheckProjection {
                    context: "bui\u{0007}ld".to_owned(),
                }])
                .collect(),
        };

        let checks = projection.into_required_checks();

        assert_eq!(checks.len(), MAX_REQUIRED_CHECK_COUNT);
        assert!(checks.contains("check-000"));
        assert!(!checks.contains("bui\u{0007}ld"));
    }
}
