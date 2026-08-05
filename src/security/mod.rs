mod admin_auth;
mod secret_cipher;
mod webhook_auth;

use std::fmt;

pub use admin_auth::{AdminAuthenticator, AuthenticationError};
pub use secret_cipher::RepositorySecretCipher;
pub use webhook_auth::{WebhookAuthenticationError, WebhookAuthenticator, WebhookSignature};

use secrecy::{zeroize::Zeroizing, ExposeSecret, SecretBox, SecretSlice, SecretString};
use thiserror::Error;

const MASTER_KEY_LENGTH: usize = 32;
const NONCE_LENGTH: usize = 12;
const GCM_TAG_LENGTH: usize = 16;
const MAX_REPOSITORY_SECRET_LENGTH: usize = 65_536;
const MAX_REPOSITORY_NAME_SEGMENT_LENGTH: usize = 100;

/// Encryption format version emitted and accepted by the repository-secret cipher.
pub const REPOSITORY_SECRET_ENCRYPTION_VERSION: u8 = 1;

/// A validated, zeroizing database-encryption root key.
pub struct MasterKey(SecretBox<[u8; MASTER_KEY_LENGTH]>);

impl MasterKey {
    /// Copies a 32-byte root key into zeroizing secret storage.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityError::InvalidMasterKeyLength`] unless `bytes` contains exactly 32 bytes.
    pub fn from_slice(bytes: &[u8]) -> Result<Self, SecurityError> {
        if bytes.len() != MASTER_KEY_LENGTH {
            return Err(SecurityError::InvalidMasterKeyLength);
        }

        Ok(Self(SecretBox::init_with_mut(
            |key: &mut [u8; MASTER_KEY_LENGTH]| key.copy_from_slice(bytes),
        )))
    }

    pub(crate) fn expose_secret(&self) -> &[u8; MASTER_KEY_LENGTH] {
        self.0.expose_secret()
    }
}

impl fmt::Debug for MasterKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("MasterKey([REDACTED])")
    }
}

/// A validated, zeroizing configuration API credential.
pub struct AdminToken(SecretString);

impl AdminToken {
    /// Stores a non-empty administrator token in zeroizing secret storage.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityError::InvalidAdminToken`] when `token` is empty.
    pub fn new(token: String) -> Result<Self, SecurityError> {
        if token.is_empty() {
            return Err(SecurityError::InvalidAdminToken);
        }

        Ok(Self(SecretString::from(token)))
    }

    pub(crate) fn expose_secret(&self) -> &str {
        self.0.expose_secret()
    }
}

impl fmt::Debug for AdminToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("AdminToken([REDACTED])")
    }
}

/// A validated plaintext webhook secret held in zeroizing memory.
pub struct RepositorySecret(SecretSlice<u8>);

impl RepositorySecret {
    /// Stores a repository webhook secret after validating its byte length.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityError::InvalidRepositorySecretLength`] when `secret` is empty or exceeds
    /// 65,536 bytes.
    pub fn new(secret: String) -> Result<Self, SecurityError> {
        if !valid_repository_secret_bytes(secret.as_bytes()) {
            return Err(SecurityError::InvalidRepositorySecretLength);
        }

        Ok(Self(SecretSlice::from(secret.into_bytes())))
    }

    pub(crate) fn from_decrypted_bytes(
        mut secret: Zeroizing<Vec<u8>>,
    ) -> Result<Self, SecurityError> {
        if !valid_repository_secret_bytes(&secret) || std::str::from_utf8(&secret).is_err() {
            return Err(SecurityError::DecryptionFailed);
        }

        Ok(Self(SecretSlice::from(std::mem::take(&mut *secret))))
    }

    /// Explicitly exposes the plaintext secret to cryptographic consumers.
    ///
    /// Callers must not format, serialize, or log the returned value.
    pub fn expose_secret(&self) -> &str {
        std::str::from_utf8(self.0.expose_secret())
            .expect("RepositorySecret validates UTF-8 at construction")
    }
}

impl fmt::Debug for RepositorySecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RepositorySecret([REDACTED])")
    }
}

/// A normalized repository name suitable for use as authenticated associated data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalRepositoryName(String);

impl CanonicalRepositoryName {
    /// Trims and lowercases a GitHub `owner/repository` name and validates both segments.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityError::InvalidRepositoryName`] unless the name has exactly two segments,
    /// each containing 1-100 ASCII alphanumeric, hyphen, underscore, or period characters.
    pub fn new(full_name: &str) -> Result<Self, SecurityError> {
        let canonical = full_name.trim().to_ascii_lowercase();
        let mut segments = canonical.split('/');
        let owner = segments
            .next()
            .ok_or(SecurityError::InvalidRepositoryName)?;
        let repository = segments
            .next()
            .ok_or(SecurityError::InvalidRepositoryName)?;
        if segments.next().is_some()
            || !valid_repository_segment(owner)
            || !valid_repository_segment(repository)
        {
            return Err(SecurityError::InvalidRepositoryName);
        }

        Ok(Self(canonical))
    }

    /// Returns the canonical lowercase name used as encryption associated data.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Storage-safe authenticated ciphertext for one repository webhook secret.
pub struct EncryptedRepositorySecret {
    version: u8,
    nonce: [u8; NONCE_LENGTH],
    ciphertext: Vec<u8>,
}

impl EncryptedRepositorySecret {
    /// Reconstructs an encrypted secret from persistence fields.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityError::UnsupportedEncryptionVersion`] for versions other than version 1,
    /// or [`SecurityError::InvalidEncryptedSecret`] for a nonce of the wrong length or ciphertext
    /// too short to contain an AES-GCM authentication tag.
    pub fn from_parts(
        version: u8,
        nonce: &[u8],
        ciphertext: Vec<u8>,
    ) -> Result<Self, SecurityError> {
        if version != REPOSITORY_SECRET_ENCRYPTION_VERSION {
            return Err(SecurityError::UnsupportedEncryptionVersion { version });
        }
        let nonce = nonce
            .try_into()
            .map_err(|_| SecurityError::InvalidEncryptedSecret)?;
        if ciphertext.len() < GCM_TAG_LENGTH {
            return Err(SecurityError::InvalidEncryptedSecret);
        }

        Ok(Self {
            version,
            nonce,
            ciphertext,
        })
    }

    /// Returns the encryption format version.
    pub fn version(&self) -> u8 {
        self.version
    }

    /// Returns the 96-bit AES-GCM nonce for database storage.
    pub fn nonce(&self) -> &[u8; NONCE_LENGTH] {
        &self.nonce
    }

    /// Returns the authenticated ciphertext for database storage.
    pub fn ciphertext(&self) -> &[u8] {
        &self.ciphertext
    }
}

impl fmt::Debug for EncryptedRepositorySecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EncryptedRepositorySecret")
            .field("version", &self.version)
            .field("nonce", &"[REDACTED]")
            .field("ciphertext", &"[REDACTED]")
            .finish()
    }
}

/// A redacted validation or cryptographic security failure.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SecurityError {
    /// The database-encryption root key was not exactly 32 bytes.
    #[error("master key must contain exactly 32 bytes")]
    InvalidMasterKeyLength,
    /// The administrator credential was empty.
    #[error("administrator token must not be empty")]
    InvalidAdminToken,
    /// The repository webhook secret was empty or larger than 65,536 bytes.
    #[error("repository secret has an invalid length")]
    InvalidRepositorySecretLength,
    /// The repository name was not a valid canonicalizable `owner/repository` value.
    #[error("repository name is invalid")]
    InvalidRepositoryName,
    /// Persisted encrypted fields did not form a valid encrypted-secret value.
    #[error("encrypted repository secret is invalid")]
    InvalidEncryptedSecret,
    /// The encrypted value uses a format this binary does not support.
    #[error("repository secret encryption version {version} is unsupported")]
    UnsupportedEncryptionVersion {
        /// Unsupported format version read from storage.
        version: u8,
    },
    /// Version-1 key derivation failed.
    #[error("repository secret key derivation failed")]
    KeyDerivationFailed,
    /// AES-GCM encryption failed without exposing secret material.
    #[error("repository secret encryption failed")]
    EncryptionFailed,
    /// AES-GCM authentication or decryption failed without exposing secret material.
    #[error("repository secret decryption failed")]
    DecryptionFailed,
}

fn valid_repository_secret_bytes(secret: &[u8]) -> bool {
    !secret.is_empty() && secret.len() <= MAX_REPOSITORY_SECRET_LENGTH
}

fn valid_repository_segment(segment: &str) -> bool {
    !segment.is_empty()
        && segment.len() <= MAX_REPOSITORY_NAME_SEGMENT_LENGTH
        && segment
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use std::fmt;

    use secrecy::zeroize::Zeroizing;
    use static_assertions::assert_not_impl_any;

    use super::{
        AdminToken, CanonicalRepositoryName, EncryptedRepositorySecret, MasterKey,
        RepositorySecret, SecurityError,
    };

    assert_not_impl_any!(MasterKey: serde::Serialize, fmt::Display);
    assert_not_impl_any!(AdminToken: serde::Serialize, fmt::Display);
    assert_not_impl_any!(RepositorySecret: serde::Serialize, fmt::Display);

    #[test]
    fn master_key_requires_exactly_32_bytes() {
        assert!(MasterKey::from_slice(&[7_u8; 32]).is_ok());
        assert_eq!(
            MasterKey::from_slice(&[7_u8; 31]).expect_err("short keys must be rejected"),
            SecurityError::InvalidMasterKeyLength
        );
        assert_eq!(
            MasterKey::from_slice(&[7_u8; 33]).expect_err("long keys must be rejected"),
            SecurityError::InvalidMasterKeyLength
        );
    }

    #[test]
    fn admin_token_rejects_empty_credentials() {
        assert_eq!(
            AdminToken::new(String::new()).expect_err("empty tokens must be rejected"),
            SecurityError::InvalidAdminToken
        );
    }

    #[test]
    fn decrypted_bytes_transfer_directly_into_zeroizing_secret_storage() {
        let mut decrypted = Zeroizing::new(b"webhook-secret".to_vec());
        decrypted.shrink_to_fit();
        let original_allocation = decrypted.as_ptr();

        let secret = RepositorySecret::from_decrypted_bytes(decrypted)
            .expect("valid decrypted bytes must be accepted");

        assert_eq!(secret.expose_secret(), "webhook-secret");
        assert_eq!(secret.expose_secret().as_ptr(), original_allocation);
    }

    #[test]
    fn repository_secret_enforces_size_boundaries() {
        assert_eq!(
            RepositorySecret::new(String::new()).expect_err("empty secrets must be rejected"),
            SecurityError::InvalidRepositorySecretLength
        );
        assert!(RepositorySecret::new("x".repeat(65_536)).is_ok());
        assert_eq!(
            RepositorySecret::new("x".repeat(65_537))
                .expect_err("oversized secrets must be rejected"),
            SecurityError::InvalidRepositorySecretLength
        );
    }

    #[test]
    fn repository_name_is_trimmed_lowercase_and_validated() {
        let name = CanonicalRepositoryName::new(" Owner/Repository ")
            .expect("valid repository names canonicalize");

        assert_eq!(name.as_str(), "owner/repository");
        for invalid in [
            "owner",
            "owner/repository/extra",
            "/repository",
            "owner/",
            "owner/repo space",
        ] {
            assert_eq!(
                CanonicalRepositoryName::new(invalid)
                    .expect_err("invalid repository names must be rejected"),
                SecurityError::InvalidRepositoryName
            );
        }
        assert!(CanonicalRepositoryName::new(&format!("{}/r", "o".repeat(100))).is_ok());
        assert_eq!(
            CanonicalRepositoryName::new(&format!("{}/r", "o".repeat(101)))
                .expect_err("segments over 100 bytes must be rejected"),
            SecurityError::InvalidRepositoryName
        );
    }

    #[test]
    fn encrypted_value_validates_storage_fields_and_redacts_debug_output() {
        assert_eq!(
            EncryptedRepositorySecret::from_parts(2, &[0_u8; 12], vec![0_u8; 16])
                .expect_err("unsupported versions must be rejected at construction"),
            SecurityError::UnsupportedEncryptionVersion { version: 2 }
        );
        assert_eq!(
            EncryptedRepositorySecret::from_parts(1, &[0_u8; 11], vec![0_u8; 16])
                .expect_err("invalid nonces must be rejected"),
            SecurityError::InvalidEncryptedSecret
        );
        assert_eq!(
            EncryptedRepositorySecret::from_parts(1, &[0_u8; 12], vec![0_u8; 15])
                .expect_err("truncated ciphertext must be rejected"),
            SecurityError::InvalidEncryptedSecret
        );
        let encrypted = EncryptedRepositorySecret::from_parts(1, &[42_u8; 12], vec![99_u8; 16])
            .expect("storage fields are valid");
        let rendered = format!("{encrypted:?}");

        assert!(!rendered.contains("42"));
        assert!(!rendered.contains("99"));
        assert!(rendered.contains("[REDACTED]"));
    }

    #[test]
    fn secret_types_and_errors_are_redacted() {
        const MASTER_KEY_TEXT: &str = "master-key-sensitive-value";
        const ADMIN_TOKEN_TEXT: &str = "admin-token-sensitive-value";
        const REPOSITORY_SECRET_TEXT: &str = "repository-secret-sensitive-value";
        let mut master_key_bytes = [0_u8; 32];
        master_key_bytes[..MASTER_KEY_TEXT.len()].copy_from_slice(MASTER_KEY_TEXT.as_bytes());
        let master_key = MasterKey::from_slice(&master_key_bytes).expect("key length is valid");
        let admin_token = AdminToken::new(ADMIN_TOKEN_TEXT.to_owned()).expect("token is valid");
        let repository_secret =
            RepositorySecret::new(REPOSITORY_SECRET_TEXT.to_owned()).expect("secret is valid");

        for rendered in [
            format!("{master_key:?}"),
            format!("{admin_token:?}"),
            format!("{repository_secret:?}"),
            SecurityError::DecryptionFailed.to_string(),
        ] {
            assert!(!rendered.contains(MASTER_KEY_TEXT));
            assert!(!rendered.contains(ADMIN_TOKEN_TEXT));
            assert!(!rendered.contains(REPOSITORY_SECRET_TEXT));
        }
    }
}
