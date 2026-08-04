use std::fmt;

use secrecy::{ExposeSecret, SecretBox};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use thiserror::Error;

use super::AdminToken;

const AUTHORIZATION_SCHEME: &str = "Bearer ";
const TOKEN_DIGEST_LENGTH: usize = 32;

/// Constant-time verifier for the independent administrator bearer credential.
pub struct AdminAuthenticator {
    expected_digest: SecretBox<[u8; TOKEN_DIGEST_LENGTH]>,
}

impl AdminAuthenticator {
    /// Builds an authenticator from a validated, zeroizing plaintext token.
    ///
    /// Authentication retains only a SHA-256 verifier, not reusable plaintext credentials.
    pub fn new(admin_token: &AdminToken) -> Self {
        let expected_digest = token_digest(admin_token.expose_secret());
        Self {
            expected_digest: SecretBox::init_with_mut(|stored: &mut [u8; TOKEN_DIGEST_LENGTH]| {
                stored.copy_from_slice(expected_digest.as_ref());
            }),
        }
    }

    /// Authenticates an optional HTTP `Authorization` value.
    ///
    /// Only an exact `Bearer <token>` value succeeds. Missing, malformed, and incorrect values all
    /// produce the same [`AuthenticationError`]. Credential digests have a fixed length and are
    /// compared with [`ConstantTimeEq`].
    ///
    /// # Errors
    ///
    /// Returns [`AuthenticationError`] for every authentication failure without identifying the
    /// failure category or embedding credential data.
    pub fn authenticate(&self, authorization: Option<&str>) -> Result<(), AuthenticationError> {
        let supplied_token = authorization
            .and_then(|value| value.strip_prefix(AUTHORIZATION_SCHEME))
            .filter(|token| !token.is_empty())
            .ok_or(AuthenticationError)?;
        let supplied_digest = token_digest(supplied_token);

        if bool::from(
            self.expected_digest
                .expose_secret()
                .ct_eq(supplied_digest.as_ref()),
        ) {
            Ok(())
        } else {
            Err(AuthenticationError)
        }
    }
}

impl fmt::Debug for AdminAuthenticator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AdminAuthenticator([REDACTED])")
    }
}

/// A deliberately uniform and redacted administrator-authentication failure.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("invalid bearer credentials")]
pub struct AuthenticationError;

fn token_digest(token: &str) -> [u8; TOKEN_DIGEST_LENGTH] {
    Sha256::digest(token.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::{AdminAuthenticator, AuthenticationError};
    use crate::security::AdminToken;

    const ADMIN_TOKEN: &str = "independent-admin-token";

    fn authenticator() -> AdminAuthenticator {
        let token = AdminToken::new(ADMIN_TOKEN.to_owned()).expect("test token is valid");
        AdminAuthenticator::new(&token)
    }

    #[test]
    fn exact_bearer_token_authenticates() {
        assert_eq!(
            authenticator().authenticate(Some("Bearer independent-admin-token")),
            Ok(())
        );
    }

    #[test]
    fn incorrect_truncated_extended_and_master_key_tokens_fail_identically() {
        for authorization in [
            "Bearer incorrect-token",
            "Bearer independent-admin-toke",
            "Bearer independent-admin-token-extra",
            "Bearer AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        ] {
            assert_eq!(
                authenticator().authenticate(Some(authorization)),
                Err(AuthenticationError)
            );
        }
    }

    #[test]
    fn missing_and_malformed_authorization_fail_identically() {
        for authorization in [
            None,
            Some(""),
            Some("Bearer"),
            Some("Bearer "),
            Some("bearer independent-admin-token"),
            Some("Basic independent-admin-token"),
        ] {
            assert_eq!(
                authenticator().authenticate(authorization),
                Err(AuthenticationError)
            );
        }
    }

    #[test]
    fn authentication_failures_and_authenticator_debug_are_redacted() {
        let error = authenticator()
            .authenticate(Some("Bearer supplied-sensitive-value"))
            .expect_err("wrong tokens must fail");

        for rendered in [error.to_string(), format!("{:?}", authenticator())] {
            assert!(!rendered.contains(ADMIN_TOKEN));
            assert!(!rendered.contains("supplied-sensitive-value"));
        }
    }
}
