use std::fmt;

use hmac::{Hmac, Mac};
use sha2::Sha256;
use thiserror::Error;

use crate::storage::{RepositoryStore, RepositoryStoreError};

use super::CanonicalRepositoryName;

const SIGNATURE_PREFIX: &[u8] = b"sha256=";
const SHA256_DIGEST_LENGTH: usize = 32;
const ENCODED_SHA256_DIGEST_LENGTH: usize = SHA256_DIGEST_LENGTH * 2;

/// A parsed GitHub `X-Hub-Signature-256` digest.
#[derive(PartialEq, Eq)]
pub struct WebhookSignature([u8; SHA256_DIGEST_LENGTH]);

impl WebhookSignature {
    /// Parses an exact `sha256=` prefix followed by 64 hexadecimal characters.
    ///
    /// # Errors
    ///
    /// Returns [`WebhookAuthenticationError::Unauthorized`] for every malformed value. The error
    /// does not retain or render any portion of `value`.
    pub fn parse(value: &str) -> Result<Self, WebhookAuthenticationError> {
        let encoded_digest = value
            .as_bytes()
            .strip_prefix(SIGNATURE_PREFIX)
            .filter(|digest| digest.len() == ENCODED_SHA256_DIGEST_LENGTH)
            .ok_or(WebhookAuthenticationError::Unauthorized)?;
        let mut digest = [0_u8; SHA256_DIGEST_LENGTH];
        hex::decode_to_slice(encoded_digest, &mut digest)
            .map_err(|_| WebhookAuthenticationError::Unauthorized)?;
        Ok(Self(digest))
    }
}

impl fmt::Debug for WebhookSignature {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WebhookSignature([REDACTED])")
    }
}

/// Verifies GitHub webhook request bodies against enabled repository secrets.
pub struct WebhookAuthenticator<'store> {
    store: &'store RepositoryStore,
}

impl<'store> WebhookAuthenticator<'store> {
    /// Creates an authenticator backed by the enabled-repository store query.
    pub fn new(store: &'store RepositoryStore) -> Self {
        Self { store }
    }

    /// Authenticates the exact borrowed request bytes for one canonical repository candidate.
    ///
    /// # Errors
    ///
    /// Returns [`WebhookAuthenticationError::Unauthorized`] for unknown or disabled repositories,
    /// wrong secrets, or mismatched signatures. Returns
    /// [`WebhookAuthenticationError::Unavailable`] when authentication material cannot be loaded
    /// or decrypted. Neither failure contains repository, request, signature, or secret values.
    pub async fn authenticate(
        &self,
        repository_name: &CanonicalRepositoryName,
        signature: &WebhookSignature,
        request_body: &[u8],
    ) -> Result<(), WebhookAuthenticationError> {
        let secret = self
            .store
            .authentication_secret(repository_name)
            .await
            .map_err(WebhookAuthenticationError::from_store_error)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.expose_secret().as_bytes())
            .map_err(|_| WebhookAuthenticationError::Unavailable)?;
        mac.update(request_body);
        mac.verify_slice(&signature.0)
            .map_err(|_| WebhookAuthenticationError::Unauthorized)
    }
}

impl fmt::Debug for WebhookAuthenticator<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("WebhookAuthenticator([REDACTED])")
    }
}

/// A stable, redacted webhook-authentication result.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WebhookAuthenticationError {
    /// The candidate repository or supplied signature was not authorized.
    #[error("webhook authentication failed")]
    Unauthorized,
    /// Authentication material could not be loaded or decrypted.
    #[error("webhook authentication is unavailable")]
    Unavailable,
}

impl WebhookAuthenticationError {
    fn from_store_error(error: RepositoryStoreError) -> Self {
        match error {
            RepositoryStoreError::AuthenticationFailed => Self::Unauthorized,
            RepositoryStoreError::NotFound
            | RepositoryStoreError::Conflict
            | RepositoryStoreError::EmptyMutation
            | RepositoryStoreError::Cryptographic(_)
            | RepositoryStoreError::Unavailable
            | RepositoryStoreError::InternalData
            | RepositoryStoreError::Internal(_) => Self::Unavailable,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use static_assertions::assert_not_impl_any;

    use crate::{
        security::{CanonicalRepositoryName, MasterKey, RepositorySecret, RepositorySecretCipher},
        storage::{open_database, RepositoryStore},
    };

    use super::{WebhookAuthenticationError, WebhookAuthenticator, WebhookSignature};

    const GITHUB_SECRET: &str = "It's a Secret to Everybody";
    const GITHUB_PAYLOAD: &[u8] = b"Hello, World!";
    const GITHUB_SIGNATURE: &str =
        "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

    assert_not_impl_any!(WebhookSignature: serde::Serialize, fmt::Display);

    fn name(value: &str) -> CanonicalRepositoryName {
        CanonicalRepositoryName::new(value).expect("test repository name is valid")
    }

    fn secret(value: &str) -> RepositorySecret {
        RepositorySecret::new(value.to_owned()).expect("test repository secret is valid")
    }

    async fn test_store() -> (tempfile::TempDir, sqlx::SqlitePool, RepositoryStore) {
        let directory = tempfile::tempdir().expect("temporary directory is created");
        let pool = open_database(&directory.path().join("exporter.sqlite3"))
            .await
            .expect("database opens and migrates");
        let cipher = RepositorySecretCipher::new(
            &MasterKey::from_slice(&[7_u8; 32]).expect("test master key is valid"),
        )
        .expect("repository cipher is created");
        let store = RepositoryStore::new(pool.clone(), cipher);
        (directory, pool, store)
    }

    #[test]
    fn signature_parser_accepts_exact_lowercase_prefix_and_hex_digest() {
        let lowercase = WebhookSignature::parse(GITHUB_SIGNATURE)
            .expect("lowercase hexadecimal signature is valid");
        let uppercase_digest = WebhookSignature::parse(
            "sha256=757107EA0EB2509FC211221CCE984B8A37570B6D7586C22C46F4379C8B043E17",
        )
        .expect("uppercase hexadecimal digest is valid");

        assert_eq!(lowercase, uppercase_digest);
    }

    #[test]
    fn signature_parser_rejects_prefix_length_and_hex_violations_identically() {
        for invalid in [
            "SHA256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17",
            "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e1",
            "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e170",
            "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e1g",
            "757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17",
        ] {
            assert_eq!(
                WebhookSignature::parse(invalid)
                    .expect_err("malformed signatures must be rejected"),
                WebhookAuthenticationError::Unauthorized
            );
        }
    }

    #[tokio::test]
    async fn authenticator_matches_the_official_github_sha256_fixture() {
        let (_directory, _pool, store) = test_store().await;
        let repository_name = name("github/docs");
        store
            .create(repository_name.clone(), secret(GITHUB_SECRET), true)
            .await
            .expect("repository is created");
        let signature =
            WebhookSignature::parse(GITHUB_SIGNATURE).expect("official GitHub signature is valid");

        WebhookAuthenticator::new(&store)
            .authenticate(&repository_name, &signature, GITHUB_PAYLOAD)
            .await
            .expect("official GitHub fixture authenticates");
    }

    #[tokio::test]
    async fn authentication_failures_are_indistinguishable_and_byte_sensitive() {
        let (_directory, _pool, store) = test_store().await;
        let enabled_name = name("owner/enabled");
        let disabled_name = name("owner/disabled");
        let wrong_secret_name = name("owner/wrong-secret");
        store
            .create(enabled_name.clone(), secret(GITHUB_SECRET), true)
            .await
            .expect("enabled repository is created");
        store
            .create(disabled_name.clone(), secret(GITHUB_SECRET), false)
            .await
            .expect("disabled repository is created");
        store
            .create(wrong_secret_name.clone(), secret("wrong-secret"), true)
            .await
            .expect("wrong-secret repository is created");
        let signature =
            WebhookSignature::parse(GITHUB_SIGNATURE).expect("official GitHub signature is valid");
        let changed_signature = WebhookSignature::parse(
            "sha256=657107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17",
        )
        .expect("changed digest remains structurally valid");
        let authenticator = WebhookAuthenticator::new(&store);

        let failures = [
            authenticator
                .authenticate(&name("owner/unknown"), &signature, GITHUB_PAYLOAD)
                .await,
            authenticator
                .authenticate(&disabled_name, &signature, GITHUB_PAYLOAD)
                .await,
            authenticator
                .authenticate(&wrong_secret_name, &signature, GITHUB_PAYLOAD)
                .await,
            authenticator
                .authenticate(&enabled_name, &signature, b"Hello, World?")
                .await,
            authenticator
                .authenticate(&enabled_name, &changed_signature, GITHUB_PAYLOAD)
                .await,
        ];

        for failure in failures {
            assert_eq!(failure, Err(WebhookAuthenticationError::Unauthorized));
        }
    }

    #[tokio::test]
    async fn storage_failures_are_redacted_and_distinct_from_rejections() {
        let (_directory, pool, store) = test_store().await;
        let repository_name = name("owner/unavailable");
        let signature =
            WebhookSignature::parse(GITHUB_SIGNATURE).expect("official GitHub signature is valid");
        pool.close().await;

        assert_eq!(
            WebhookAuthenticator::new(&store)
                .authenticate(&repository_name, &signature, GITHUB_PAYLOAD)
                .await,
            Err(WebhookAuthenticationError::Unavailable)
        );
    }

    #[tokio::test]
    async fn authentication_debug_and_errors_reveal_no_sensitive_or_attacker_values() {
        const REPOSITORY_NAME: &str = "sensitive-owner/sensitive-repository";
        const SECRET: &str = "sensitive-webhook-secret";
        const PAYLOAD: &[u8] = b"sensitive-payload-content";
        const SIGNATURE: &str =
            "sha256=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let (_directory, _pool, store) = test_store().await;
        let repository_name = name(REPOSITORY_NAME);
        store
            .create(repository_name.clone(), secret(SECRET), true)
            .await
            .expect("repository is created");
        let signature = WebhookSignature::parse(SIGNATURE).expect("test signature is valid");
        let authenticator = WebhookAuthenticator::new(&store);
        let error = authenticator
            .authenticate(&repository_name, &signature, PAYLOAD)
            .await
            .expect_err("incorrect signature must fail");

        for rendered in [
            format!("{signature:?}"),
            format!("{authenticator:?}"),
            error.to_string(),
            format!("{error:?}"),
        ] {
            for forbidden in [
                REPOSITORY_NAME,
                SECRET,
                std::str::from_utf8(PAYLOAD).expect("payload is UTF-8"),
                SIGNATURE,
                &SIGNATURE[7..],
            ] {
                assert!(!rendered.contains(forbidden));
            }
        }
    }
}
