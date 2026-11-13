//! GitHub Actions OIDC verification for in-job cost reports.
//!
//! A job proves which repository, workflow and run it belongs to by presenting the OIDC token
//! GitHub minted for it. Every attribution field comes from the verified claims, never from the
//! request body.

use std::{
    collections::{HashMap, HashSet},
    fmt,
    sync::Arc,
    time::Duration,
};

use async_trait::async_trait;
use jsonwebtoken::{
    decode, decode_header, errors::ErrorKind, jwk::JwkSet, Algorithm, DecodingKey, Validation,
};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use tokio::{sync::Mutex, time::Instant};

type KeyLookup = Result<Arc<DecodingKey>, OidcRejection>;

use crate::{
    config::ConfigError,
    security::CanonicalRepositoryName,
    telemetry::{
        trace::CommitSha,
        workflow::{DisplayName, WorkflowEvent, WorkflowFile, WorkflowRunAttempt, WorkflowRunId},
    },
};

/// Environment variable carrying the JSON `ingest.oidc` block.
pub const INGEST_OIDC_VARIABLE: &str = "GHE_INGEST_OIDC_JSON";
/// The only issuer whose tokens are accepted.
pub const GITHUB_ACTIONS_ISSUER: &str = "https://token.actions.githubusercontent.com";
const GITHUB_ACTIONS_JWKS_URL: &str =
    "https://token.actions.githubusercontent.com/.well-known/jwks";
const MAX_AUDIENCE_LENGTH: usize = 255;
const MAX_ALLOWED_REPOSITORIES: usize = 256;
const MAX_JWKS_BYTES: usize = 65_536;
const JWKS_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// How long fetched keys are trusted before the next verification refetches them.
const JWKS_MAX_AGE: Duration = Duration::from_secs(3_600);
/// The shortest gap between two fetches, so tokens with unknown `kid`s cannot hammer GitHub.
const JWKS_MIN_REFETCH_INTERVAL: Duration = Duration::from_secs(60);
const CLOCK_SKEW_LEEWAY_SECONDS: u64 = 60;
const USER_AGENT: &str = concat!("github-webhook-exporter/", env!("CARGO_PKG_VERSION"));

/// Validated `ingest.oidc` configuration: the expected audience and the repositories allowed to
/// report usage.
#[derive(Clone)]
pub struct IngestOidcConfig {
    audience: String,
    allowed_repositories: HashSet<CanonicalRepositoryName>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IngestOidcWire {
    audience: String,
    allowed_repositories: Vec<String>,
}

impl IngestOidcConfig {
    /// Parses and validates the JSON `ingest.oidc` block.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] naming [`INGEST_OIDC_VARIABLE`] when the JSON is malformed
    /// or has an unknown field, the audience is empty or longer than 255 bytes, or the repository
    /// list is empty, too long, or holds an invalid `owner/repo` name. Names that differ only in
    /// case are one repository, as GitHub treats them, so they are merged rather than rejected.
    /// Parser messages are discarded because they can quote configured values.
    pub fn from_json(json: &str) -> Result<Self, ConfigError> {
        let invalid = || ConfigError::Invalid {
            variable: INGEST_OIDC_VARIABLE,
        };
        let wire: IngestOidcWire = serde_json::from_str(json).map_err(|_| invalid())?;
        if wire.audience.is_empty()
            || wire.audience.len() > MAX_AUDIENCE_LENGTH
            || wire.audience.chars().any(char::is_control)
            || wire.allowed_repositories.is_empty()
            || wire.allowed_repositories.len() > MAX_ALLOWED_REPOSITORIES
        {
            return Err(invalid());
        }
        let mut allowed_repositories = HashSet::new();
        for name in &wire.allowed_repositories {
            allowed_repositories.insert(CanonicalRepositoryName::new(name).map_err(|_| invalid())?);
        }
        Ok(Self {
            audience: wire.audience,
            allowed_repositories,
        })
    }

    /// Returns the number of allowlisted repositories.
    pub fn allowed_repository_count(&self) -> usize {
        self.allowed_repositories.len()
    }

    fn allows(&self, repository: &CanonicalRepositoryName) -> bool {
        self.allowed_repositories.contains(repository)
    }
}

impl fmt::Debug for IngestOidcConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IngestOidcConfig")
            .field("allowed_repositories", &self.allowed_repositories.len())
            .finish_non_exhaustive()
    }
}

/// The bearer token presented by one usage post, held so it never reaches a log or `Debug`.
pub(crate) struct OidcToken(SecretString);

impl OidcToken {
    /// Reads the token from an `Authorization` header value.
    ///
    /// Returns `None` unless the value is `Bearer <token>` with a non-empty token.
    pub(crate) fn from_authorization(value: &str) -> Option<Self> {
        let (scheme, token) = value.split_once(' ')?;
        let token = token.trim();
        (scheme.eq_ignore_ascii_case("bearer") && !token.is_empty())
            .then(|| Self(SecretString::from(token.to_owned())))
    }
}

impl fmt::Debug for OidcToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("OidcToken([REDACTED])")
    }
}

/// Attribution read from a verified GitHub Actions OIDC token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GitHubActionsClaims {
    pub(crate) repository: CanonicalRepositoryName,
    /// GitHub's immutable numeric repository ID. Unlike the name, it survives renames and
    /// transfers and is never reused, so usage is pinned to it.
    pub(crate) github_repository_id: i64,
    pub(crate) workflow: Option<DisplayName>,
    pub(crate) workflow_file: Option<WorkflowFile>,
    pub(crate) job_workflow_file: Option<WorkflowFile>,
    pub(crate) run_id: WorkflowRunId,
    pub(crate) run_attempt: WorkflowRunAttempt,
    pub(crate) event: WorkflowEvent,
    pub(crate) sha: Option<CommitSha>,
}

/// The claims this service reads. GitHub encodes the IDs and attempt as strings.
#[derive(Deserialize)]
struct RawClaims {
    repository: String,
    repository_id: String,
    #[serde(default)]
    workflow: Option<String>,
    #[serde(default)]
    workflow_ref: Option<String>,
    #[serde(default)]
    job_workflow_ref: Option<String>,
    run_id: String,
    run_attempt: String,
    #[serde(default)]
    event_name: Option<String>,
    #[serde(default)]
    sha: Option<String>,
}

impl RawClaims {
    fn into_claims(self) -> Option<GitHubActionsClaims> {
        Some(GitHubActionsClaims {
            repository: CanonicalRepositoryName::new(&self.repository).ok()?,
            github_repository_id: self.repository_id.parse().ok().filter(|id: &i64| *id > 0)?,
            workflow: self.workflow.as_deref().and_then(DisplayName::sanitize),
            workflow_file: self
                .workflow_ref
                .as_deref()
                .and_then(workflow_file_from_ref),
            job_workflow_file: self
                .job_workflow_ref
                .as_deref()
                .and_then(workflow_file_from_ref),
            run_id: WorkflowRunId::new(self.run_id.parse().ok()?).ok()?,
            run_attempt: WorkflowRunAttempt::new(self.run_attempt.parse().ok()?).ok()?,
            event: WorkflowEvent::normalize(self.event_name.as_deref()),
            sha: self
                .sha
                .as_deref()
                .and_then(|sha| CommitSha::parse(sha).ok()),
        })
    }
}

/// Reads the workflow file name from a ref such as
/// `octo/repo/.github/workflows/ci.yml@refs/heads/main`.
///
/// The ref after `@` can itself contain `@`, so the split prefers the `@refs/` boundary.
pub(crate) fn workflow_file_from_ref(workflow_ref: &str) -> Option<WorkflowFile> {
    let path = workflow_ref
        .find("@refs/")
        .or_else(|| workflow_ref.rfind('@'))
        .map_or(workflow_ref, |at| &workflow_ref[..at]);
    WorkflowFile::from_path(path)
}

/// Why a token was refused. The values are the closed `reason` vocabulary of the
/// `ci usage post rejected` warning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OidcRejection {
    /// The token is malformed, has a bad signature, an unknown key, or unusable claims.
    InvalidToken,
    /// The token's `exp` has passed.
    TokenExpired,
    /// The token was not issued by GitHub Actions.
    IssuerMismatch,
    /// The token's `aud` is not the configured audience.
    AudienceMismatch,
    /// The token's repository is not allowlisted.
    RepositoryNotAllowed,
    /// The signing keys could not be fetched, so the token could not be checked.
    JwksUnavailable,
}

impl OidcRejection {
    /// Returns the closed rejection reason.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::InvalidToken => "invalid_token",
            Self::TokenExpired => "token_expired",
            Self::IssuerMismatch => "issuer_mismatch",
            Self::AudienceMismatch => "audience_mismatch",
            Self::RepositoryNotAllowed => "repository_not_allowed",
            Self::JwksUnavailable => "jwks_unavailable",
        }
    }
}

/// A failed JWKS fetch whose details were discarded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JwksFetchError;

/// Fetches the issuer's signing keys.
///
/// [`GitHubJwksClient`] is the only production implementation. The trait lets tests verify
/// tokens signed with a local key, without network access.
#[async_trait]
pub(crate) trait JwksSource: Send + Sync {
    /// Returns the current key set.
    async fn fetch(&self) -> Result<JwkSet, JwksFetchError>;
}

/// Fetches GitHub Actions' published JWKS.
pub(crate) struct GitHubJwksClient {
    http: reqwest::Client,
}

impl GitHubJwksClient {
    /// Builds a client with a short timeout and redirects disabled.
    pub(crate) fn new() -> Result<Self, JwksFetchError> {
        let http = reqwest::Client::builder()
            .timeout(JWKS_REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(USER_AGENT)
            .build()
            .map_err(|_| JwksFetchError)?;
        Ok(Self { http })
    }
}

#[async_trait]
impl JwksSource for GitHubJwksClient {
    async fn fetch(&self) -> Result<JwkSet, JwksFetchError> {
        let mut response = self
            .http
            .get(GITHUB_ACTIONS_JWKS_URL)
            .send()
            .await
            .map_err(|_| JwksFetchError)?;
        if !response.status().is_success()
            || response
                .content_length()
                .is_some_and(|length| length > MAX_JWKS_BYTES as u64)
        {
            return Err(JwksFetchError);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| JwksFetchError)? {
            if body.len() + chunk.len() > MAX_JWKS_BYTES {
                return Err(JwksFetchError);
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&body).map_err(|_| JwksFetchError)
    }
}

#[derive(Default)]
struct KeyCache {
    keys: HashMap<String, Arc<DecodingKey>>,
    fetched_at: Option<Instant>,
    /// When the last fetch was attempted, successful or not. The refetch interval applies to
    /// attempts, so an unreachable JWKS is not retried on every request.
    attempted_at: Option<Instant>,
    last_attempt_failed: bool,
}

impl KeyCache {
    fn is_stale(&self, now: Instant) -> bool {
        self.fetched_at
            .is_none_or(|fetched_at| now.duration_since(fetched_at) >= JWKS_MAX_AGE)
    }

    fn may_attempt(&self, now: Instant) -> bool {
        self.attempted_at.is_none_or(|attempted_at| {
            now.duration_since(attempted_at) >= JWKS_MIN_REFETCH_INTERVAL
        })
    }

    fn lookup(&self, kid: &str) -> KeyLookup {
        match self.keys.get(kid) {
            Some(key) => Ok(Arc::clone(key)),
            None if self.last_attempt_failed => Err(OidcRejection::JwksUnavailable),
            None => Err(OidcRejection::InvalidToken),
        }
    }

    fn replace(&mut self, set: &JwkSet, now: Instant) {
        self.keys = set
            .keys
            .iter()
            .filter_map(|jwk| {
                let kid = jwk.common.key_id.clone()?;
                let key = DecodingKey::from_jwk(jwk).ok()?;
                Some((kid, Arc::new(key)))
            })
            .collect();
        self.fetched_at = Some(now);
    }
}

/// Verifies GitHub Actions OIDC tokens against a cached JWKS.
pub(crate) struct OidcVerifier {
    config: IngestOidcConfig,
    source: Box<dyn JwksSource>,
    cache: std::sync::Mutex<KeyCache>,
    /// Held for the length of one JWKS fetch, so at most one runs and the cache lock is never
    /// held across the network call.
    refresh: Mutex<()>,
}

impl OidcVerifier {
    /// Creates a verifier for `config` that reads keys from `source`.
    pub(crate) fn new(config: IngestOidcConfig, source: Box<dyn JwksSource>) -> Self {
        Self {
            config,
            source,
            cache: std::sync::Mutex::new(KeyCache::default()),
            refresh: Mutex::new(()),
        }
    }

    /// Verifies the signature, issuer, audience, expiry and repository of `token`.
    ///
    /// # Errors
    ///
    /// Returns the [`OidcRejection`] that names why the token was refused.
    pub(crate) async fn verify(
        &self,
        token: &OidcToken,
    ) -> Result<GitHubActionsClaims, OidcRejection> {
        let token = token.0.expose_secret();
        let header = decode_header(token).map_err(|_| OidcRejection::InvalidToken)?;
        if header.alg != Algorithm::RS256 {
            return Err(OidcRejection::InvalidToken);
        }
        let kid = header.kid.ok_or(OidcRejection::InvalidToken)?;
        let key = self.key(&kid).await?;

        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_issuer(&[GITHUB_ACTIONS_ISSUER]);
        validation.set_audience(&[self.config.audience.as_str()]);
        validation.set_required_spec_claims(&["exp", "iss", "aud"]);
        validation.validate_nbf = true;
        validation.leeway = CLOCK_SKEW_LEEWAY_SECONDS;
        let data =
            decode::<RawClaims>(token, &key, &validation).map_err(|error| match error.kind() {
                ErrorKind::ExpiredSignature => OidcRejection::TokenExpired,
                ErrorKind::InvalidIssuer => OidcRejection::IssuerMismatch,
                ErrorKind::InvalidAudience => OidcRejection::AudienceMismatch,
                _ => OidcRejection::InvalidToken,
            })?;
        let claims = data
            .claims
            .into_claims()
            .ok_or(OidcRejection::InvalidToken)?;
        if !self.config.allows(&claims.repository) {
            return Err(OidcRejection::RepositoryNotAllowed);
        }
        Ok(claims)
    }

    /// Returns the key for `kid`, refetching when the cache is stale or lacks it.
    ///
    /// At most one fetch is attempted per [`JWKS_MIN_REFETCH_INTERVAL`], whether or not it
    /// succeeds. The fetch runs outside the cache lock: a request whose key is cached is answered
    /// from the cache while a refresh is in flight, and only a request that needs the refresh
    /// waits for it. A failed refetch keeps serving cached keys. After a successful refetch only
    /// the new key set is trusted, so a key GitHub retired stops verifying at once. A missing key
    /// is [`OidcRejection::JwksUnavailable`] while the last attempt failed, since it may be a
    /// rotated key the cache has not seen, and [`OidcRejection::InvalidToken`] otherwise.
    async fn key(&self, kid: &str) -> KeyLookup {
        let cached = {
            let cache = self.lock_cache();
            let now = Instant::now();
            let cached = cache.keys.get(kid).cloned();
            let wanted = cached.is_none() || cache.is_stale(now);
            if !wanted || !cache.may_attempt(now) {
                return cache.lookup(kid);
            }
            cached
        };
        let _refresh = match (self.refresh.try_lock(), cached) {
            (Ok(guard), _) => guard,
            (Err(_), Some(key)) => return Ok(key),
            (Err(_), None) => {
                let guard = self.refresh.lock().await;
                let cache = self.lock_cache();
                if !cache.may_attempt(Instant::now()) {
                    return cache.lookup(kid);
                }
                drop(cache);
                guard
            }
        };
        {
            let mut cache = self.lock_cache();
            let now = Instant::now();
            if !cache.may_attempt(now) {
                return cache.lookup(kid);
            }
            cache.attempted_at = Some(now);
        }
        let fetched = self.source.fetch().await;
        let mut cache = self.lock_cache();
        match fetched {
            Ok(set) => {
                cache.replace(&set, Instant::now());
                cache.last_attempt_failed = false;
            }
            Err(JwksFetchError) => cache.last_attempt_failed = true,
        }
        cache.lookup(kid)
    }

    fn lock_cache(&self) -> std::sync::MutexGuard<'_, KeyCache> {
        self.cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Local RS256 signing for tests, so no test needs network access or a committed key.

    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use async_trait::async_trait;
    use aws_lc_rs::{
        rand::SystemRandom,
        rsa::{KeyPair, KeySize},
        signature::{KeyPair as _, RSA_PKCS1_SHA256},
    };
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
    use jsonwebtoken::jwk::JwkSet;
    use serde_json::{json, Value};
    use time::OffsetDateTime;

    use super::{JwksFetchError, JwksSource, GITHUB_ACTIONS_ISSUER};

    /// The audience every test configuration expects.
    pub(crate) const TEST_AUDIENCE: &str = "https://exporter.example.test";

    /// A locally generated RSA signing key published under one `kid`.
    pub(crate) struct TestSigner {
        key_pair: KeyPair,
        kid: String,
    }

    impl TestSigner {
        pub(crate) fn new(kid: &str) -> Self {
            Self {
                key_pair: KeyPair::generate(KeySize::Rsa2048).expect("test RSA key generates"),
                kid: kid.to_owned(),
            }
        }

        /// Returns the JWKS that publishes this key.
        pub(crate) fn jwks(&self) -> JwkSet {
            let public = self.key_pair.public_key();
            serde_json::from_value(json!({"keys": [{
                "kty": "RSA",
                "use": "sig",
                "alg": "RS256",
                "kid": self.kid,
                "n": URL_SAFE_NO_PAD.encode(public.modulus().big_endian_without_leading_zero()),
                "e": URL_SAFE_NO_PAD.encode(public.exponent().big_endian_without_leading_zero()),
            }]}))
            .expect("test JWKS parses")
        }

        /// Signs `claims` as an RS256 JWT with this key's `kid`.
        pub(crate) fn sign(&self, claims: &Value) -> String {
            self.sign_with_header(
                &json!({"alg": "RS256", "typ": "JWT", "kid": self.kid}),
                claims,
            )
        }

        pub(crate) fn sign_with_header(&self, header: &Value, claims: &Value) -> String {
            let signing_input = format!(
                "{}.{}",
                URL_SAFE_NO_PAD.encode(header.to_string()),
                URL_SAFE_NO_PAD.encode(claims.to_string())
            );
            let mut signature = vec![0; self.key_pair.public_modulus_len()];
            self.key_pair
                .sign(
                    &RSA_PKCS1_SHA256,
                    &SystemRandom::new(),
                    signing_input.as_bytes(),
                    &mut signature,
                )
                .expect("test token signs");
            format!("{signing_input}.{}", URL_SAFE_NO_PAD.encode(signature))
        }
    }

    /// Claims GitHub would mint for a `pull_request` run of `ci.yml` in `repository`.
    pub(crate) fn claims(repository: &str) -> Value {
        let now = OffsetDateTime::now_utc().unix_timestamp();
        json!({
            "iss": GITHUB_ACTIONS_ISSUER,
            "aud": TEST_AUDIENCE,
            "iat": now,
            "nbf": now,
            "exp": now + 300,
            "repository": repository,
            "repository_id": "4242",
            "workflow": "CI",
            "workflow_ref": format!("{repository}/.github/workflows/ci.yml@refs/heads/main"),
            "job_workflow_ref":
                format!("{repository}/.github/workflows/evals.yml@refs/heads/main"),
            "run_id": "31",
            "run_attempt": "2",
            "event_name": "pull_request",
            "sha": "0123456789abcdef0123456789abcdef01234567",
        })
    }

    /// A JWKS source that serves fixed keys, or fails, and counts its fetches. Clones share
    /// state, so a test can make the source fail after handing it to a verifier.
    #[derive(Clone)]
    pub(crate) struct StaticJwks {
        set: Arc<std::sync::Mutex<Option<JwkSet>>>,
        fetches: Arc<AtomicUsize>,
    }

    impl StaticJwks {
        pub(crate) fn serving(set: JwkSet) -> Self {
            Self {
                set: Arc::new(std::sync::Mutex::new(Some(set))),
                fetches: Arc::default(),
            }
        }

        pub(crate) fn failing() -> Self {
            Self {
                set: Arc::default(),
                fetches: Arc::default(),
            }
        }

        pub(crate) fn start_failing(&self) {
            *self.set.lock().expect("test JWKS lock") = None;
        }

        pub(crate) fn start_serving(&self, set: JwkSet) {
            *self.set.lock().expect("test JWKS lock") = Some(set);
        }

        pub(crate) fn fetches(&self) -> usize {
            self.fetches.load(Ordering::SeqCst)
        }
    }

    /// A JWKS source whose fetches wait until the test releases them, to hold a refresh open.
    pub(crate) struct GatedJwks {
        pub(crate) inner: StaticJwks,
        pub(crate) gate: Arc<tokio::sync::Semaphore>,
    }

    #[async_trait]
    impl JwksSource for GatedJwks {
        async fn fetch(&self) -> Result<JwkSet, JwksFetchError> {
            let _permit = self.gate.acquire().await.map_err(|_| JwksFetchError)?;
            self.inner.fetch().await
        }
    }

    #[async_trait]
    impl JwksSource for StaticJwks {
        async fn fetch(&self) -> Result<JwkSet, JwksFetchError> {
            self.fetches.fetch_add(1, Ordering::SeqCst);
            self.set
                .lock()
                .expect("test JWKS lock")
                .clone()
                .ok_or(JwksFetchError)
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        test_support::{claims, StaticJwks, TestSigner, TEST_AUDIENCE},
        workflow_file_from_ref, IngestOidcConfig, OidcRejection, OidcToken, OidcVerifier,
        INGEST_OIDC_VARIABLE,
    };
    use crate::{config::ConfigError, telemetry::workflow::WorkflowEvent};

    const REPOSITORY: &str = "Owner/Repo";

    fn config() -> IngestOidcConfig {
        IngestOidcConfig::from_json(&format!(
            r#"{{"audience": "{TEST_AUDIENCE}", "allowed_repositories": ["{REPOSITORY}"]}}"#
        ))
        .expect("config is valid")
    }

    fn verifier(source: StaticJwks) -> OidcVerifier {
        OidcVerifier::new(config(), Box::new(source))
    }

    fn bearer(token: &str) -> OidcToken {
        OidcToken::from_authorization(&format!("Bearer {token}")).expect("bearer token parses")
    }

    #[tokio::test]
    async fn a_valid_token_yields_its_claims() {
        let signer = TestSigner::new("key-1");
        let verifier = verifier(StaticJwks::serving(signer.jwks()));

        let claims = verifier
            .verify(&bearer(&signer.sign(&claims("owner/repo"))))
            .await
            .expect("token verifies");

        assert_eq!(claims.repository.as_str(), "owner/repo");
        assert_eq!(claims.github_repository_id, 4242);
        assert_eq!(
            claims.workflow.as_ref().map(|name| name.as_str()),
            Some("CI")
        );
        assert_eq!(
            claims.workflow_file.as_ref().map(|file| file.as_str()),
            Some("ci.yml")
        );
        assert_eq!(
            claims.job_workflow_file.as_ref().map(|file| file.as_str()),
            Some("evals.yml")
        );
        assert_eq!(claims.run_id.get(), 31);
        assert_eq!(claims.run_attempt.get(), 2);
        assert_eq!(claims.event, WorkflowEvent::PullRequest);
        assert_eq!(
            claims.sha.as_ref().map(|sha| sha.as_str()),
            Some("0123456789abcdef0123456789abcdef01234567")
        );
    }

    #[tokio::test]
    async fn each_token_failure_maps_to_its_reason() {
        let signer = TestSigner::new("key-1");
        let other_signer = TestSigner::new("key-1");
        let verifier = verifier(StaticJwks::serving(signer.jwks()));
        let mut expired = claims("owner/repo");
        expired["exp"] = json!(1_000);
        expired["nbf"] = json!(900);
        let mut wrong_issuer = claims("owner/repo");
        wrong_issuer["iss"] = json!("https://issuer.example.test");
        let mut wrong_audience = claims("owner/repo");
        wrong_audience["aud"] = json!("https://other.example.test");
        let mut bad_run_id = claims("owner/repo");
        bad_run_id["run_id"] = json!("not-a-number");
        let mut no_repository_id = claims("owner/repo");
        no_repository_id
            .as_object_mut()
            .expect("claims are an object")
            .remove("repository_id");
        let hs256 = signer.sign_with_header(
            &json!({"alg": "HS256", "kid": "key-1"}),
            &claims("owner/repo"),
        );

        for (token, expected) in [
            (
                other_signer.sign(&claims("owner/repo")),
                OidcRejection::InvalidToken,
            ),
            (signer.sign(&expired), OidcRejection::TokenExpired),
            (signer.sign(&wrong_issuer), OidcRejection::IssuerMismatch),
            (
                signer.sign(&wrong_audience),
                OidcRejection::AudienceMismatch,
            ),
            (
                signer.sign(&claims("owner/other")),
                OidcRejection::RepositoryNotAllowed,
            ),
            (signer.sign(&bad_run_id), OidcRejection::InvalidToken),
            (signer.sign(&no_repository_id), OidcRejection::InvalidToken),
            (hs256, OidcRejection::InvalidToken),
            ("not.a.jwt".to_owned(), OidcRejection::InvalidToken),
        ] {
            assert_eq!(
                verifier.verify(&bearer(&token)).await,
                Err(expected),
                "{}",
                expected.as_str()
            );
        }
    }

    #[tokio::test]
    async fn unknown_keys_refetch_at_most_once_per_interval() {
        let signer = TestSigner::new("key-1");
        let stranger = TestSigner::new("key-2");
        let source = StaticJwks::serving(signer.jwks());
        let verifier = verifier(source.clone());

        for _ in 0..3 {
            assert_eq!(
                verifier
                    .verify(&bearer(&stranger.sign(&claims("owner/repo"))))
                    .await,
                Err(OidcRejection::InvalidToken)
            );
        }
        verifier
            .verify(&bearer(&signer.sign(&claims("owner/repo"))))
            .await
            .expect("cached key verifies");

        assert_eq!(source.fetches(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreachable_jwks_is_unavailable_and_retried_at_most_once_per_interval() {
        let signer = TestSigner::new("key-1");
        let source = StaticJwks::failing();
        let verifier = verifier(source.clone());
        let token = bearer(&signer.sign(&claims("owner/repo")));

        for _ in 0..3 {
            assert_eq!(
                verifier.verify(&token).await,
                Err(OidcRejection::JwksUnavailable)
            );
        }
        assert_eq!(source.fetches(), 1);

        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        assert_eq!(
            verifier.verify(&token).await,
            Err(OidcRejection::JwksUnavailable)
        );
        assert_eq!(source.fetches(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_rotated_key_during_a_jwks_outage_is_unavailable_not_invalid() {
        let signer = TestSigner::new("key-1");
        let rotated = TestSigner::new("key-2");
        let source = StaticJwks::serving(signer.jwks());
        let verifier = verifier(source.clone());
        verifier
            .verify(&bearer(&signer.sign(&claims("owner/repo"))))
            .await
            .expect("first key verifies");

        source.start_failing();
        tokio::time::advance(std::time::Duration::from_secs(61)).await;
        assert_eq!(
            verifier
                .verify(&bearer(&rotated.sign(&claims("owner/repo"))))
                .await,
            Err(OidcRejection::JwksUnavailable)
        );
        verifier
            .verify(&bearer(&signer.sign(&claims("owner/repo"))))
            .await
            .expect("cached key still verifies during the outage");
        assert_eq!(source.fetches(), 2);
    }

    #[tokio::test]
    async fn cached_keys_verify_while_a_refresh_is_in_flight() {
        let signer = TestSigner::new("key-1");
        let stranger = TestSigner::new("key-2");
        let inner = StaticJwks::serving(signer.jwks());
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let verifier = std::sync::Arc::new(OidcVerifier::new(
            config(),
            Box::new(super::test_support::GatedJwks {
                inner: inner.clone(),
                gate: gate.clone(),
            }),
        ));
        let token = bearer(&signer.sign(&claims("owner/repo")));
        verifier.verify(&token).await.expect("first key verifies");
        verifier.lock_cache().fetched_at = None;
        verifier.lock_cache().attempted_at = None;
        let held = gate.clone().acquire_owned().await.expect("gate is open");

        let refreshing = {
            let verifier = verifier.clone();
            let token = bearer(&stranger.sign(&claims("owner/repo")));
            tokio::spawn(async move { verifier.verify(&token).await })
        };
        while verifier.refresh.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
        tokio::time::timeout(std::time::Duration::from_secs(1), verifier.verify(&token))
            .await
            .expect("a cached key does not wait for the refresh")
            .expect("cached key verifies");

        drop(held);
        assert_eq!(
            refreshing.await.expect("refresh task completes"),
            Err(OidcRejection::InvalidToken)
        );
        assert_eq!(inner.fetches(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn a_key_retired_by_a_refresh_stops_verifying_immediately() {
        let retired = TestSigner::new("key-1");
        let current = TestSigner::new("key-2");
        let source = StaticJwks::serving(retired.jwks());
        let verifier = verifier(source.clone());
        let token = bearer(&retired.sign(&claims("owner/repo")));
        verifier.verify(&token).await.expect("first key verifies");

        tokio::time::advance(std::time::Duration::from_secs(3_601)).await;
        source.start_serving(current.jwks());

        assert_eq!(
            verifier.verify(&token).await,
            Err(OidcRejection::InvalidToken)
        );
        verifier
            .verify(&bearer(&current.sign(&claims("owner/repo"))))
            .await
            .expect("new key verifies");
        assert_eq!(source.fetches(), 2);
    }

    #[tokio::test(start_paused = true)]
    async fn stale_keys_are_refreshed_once_and_kept_when_the_refresh_fails() {
        let signer = TestSigner::new("key-1");
        let source = StaticJwks::serving(signer.jwks());
        let verifier = verifier(source.clone());
        let token = bearer(&signer.sign(&claims("owner/repo")));
        verifier.verify(&token).await.expect("token verifies");

        source.start_failing();
        tokio::time::advance(std::time::Duration::from_secs(3_601)).await;
        for _ in 0..3 {
            verifier
                .verify(&token)
                .await
                .expect("stale cached key keeps verifying");
        }
        assert_eq!(source.fetches(), 2);
    }

    #[test]
    fn workflow_files_are_read_from_refs() {
        for (workflow_ref, expected) in [
            (
                "o/r/.github/workflows/ci.yml@refs/heads/main",
                Some("ci.yml"),
            ),
            (
                "o/r/.github/workflows/ci.yml@refs/heads/fix@home",
                Some("ci.yml"),
            ),
            ("o/r/.github/workflows/ci.yml@0123abc", Some("ci.yml")),
            ("o/r/.github/workflows/ci.yml", Some("ci.yml")),
            ("o/r/.github/workflows/bad name.yml@refs/heads/main", None),
        ] {
            assert_eq!(
                workflow_file_from_ref(workflow_ref)
                    .as_ref()
                    .map(|file| file.as_str()),
                expected,
                "{workflow_ref}"
            );
        }
    }

    static_assertions::assert_not_impl_any!(OidcToken: serde::Serialize, std::fmt::Display);

    #[test]
    fn bearer_tokens_are_parsed_and_never_printed() {
        let token = OidcToken::from_authorization("bearer secret-token-value").expect("parses");
        assert_eq!(format!("{token:?}"), "OidcToken([REDACTED])");
        assert!(OidcToken::from_authorization("Basic abc").is_none());
        assert!(OidcToken::from_authorization("Bearer ").is_none());
        assert!(OidcToken::from_authorization("Bearer").is_none());
    }

    #[test]
    fn invalid_config_names_only_the_variable() {
        let invalid = Err(ConfigError::Invalid {
            variable: INGEST_OIDC_VARIABLE,
        });
        for json in [
            "not json",
            r#"{"audience": "", "allowed_repositories": ["o/r"]}"#,
            r#"{"audience": "a", "allowed_repositories": []}"#,
            r#"{"audience": "a", "allowed_repositories": ["not a repo"]}"#,
            r#"{"audience": "a", "allowed_repositories": ["o/r"], "extra": 1}"#,
        ] {
            assert_eq!(
                IngestOidcConfig::from_json(json).map(|_| ()),
                invalid,
                "{json}"
            );
        }
        let merged = IngestOidcConfig::from_json(
            r#"{"audience": "a", "allowed_repositories": ["o/r", "O/R"]}"#,
        )
        .expect("names differing only in case are one repository");
        assert_eq!(merged.allowed_repository_count(), 1);
        let rendered = format!("{:?}", config());
        assert!(!rendered.contains(TEST_AUDIENCE));
        assert!(!rendered.contains("owner/repo"));
    }
}
