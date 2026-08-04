use std::fmt;

use aes_gcm::{
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
    Aes256Gcm,
};
use hkdf::Hkdf;
use secrecy::zeroize::Zeroizing;
use sha2::Sha256;

use super::{
    CanonicalRepositoryName, EncryptedRepositorySecret, MasterKey, RepositorySecret, SecurityError,
    NONCE_LENGTH, REPOSITORY_SECRET_ENCRYPTION_VERSION,
};

const KEY_DERIVATION_CONTEXT: &[u8] = b"github-webhook-exporter/repository-secret/v1";
const DERIVED_KEY_LENGTH: usize = 32;

/// Version-1 AES-256-GCM encryption for repository webhook secrets.
pub struct RepositorySecretCipher {
    cipher: Aes256Gcm,
}

impl RepositorySecretCipher {
    /// Derives a repository-secret key from the database-encryption root key.
    ///
    /// HKDF-SHA-256 domain-separates this key with the version-1 repository-secret context.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityError::KeyDerivationFailed`] if HKDF cannot produce the fixed 32-byte key.
    pub fn new(master_key: &MasterKey) -> Result<Self, SecurityError> {
        let derived_key = derive_key(master_key)?;
        let cipher = Aes256Gcm::new_from_slice(derived_key.as_ref())
            .map_err(|_| SecurityError::KeyDerivationFailed)?;
        Ok(Self { cipher })
    }

    /// Encrypts a repository secret with a fresh operating-system CSPRNG nonce.
    ///
    /// `repository_name` is authenticated as associated data. The same canonical name must be
    /// supplied to [`Self::decrypt`], so ciphertext cannot be moved between repositories.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityError::EncryptionFailed`] if AES-GCM encryption fails.
    pub fn encrypt(
        &self,
        repository_name: &CanonicalRepositoryName,
        secret: &RepositorySecret,
    ) -> Result<EncryptedRepositorySecret, SecurityError> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: secret.expose_secret().as_bytes(),
                    aad: repository_name.as_str().as_bytes(),
                },
            )
            .map_err(|_| SecurityError::EncryptionFailed)?;
        let mut stored_nonce = [0_u8; NONCE_LENGTH];
        stored_nonce.copy_from_slice(&nonce);

        Ok(EncryptedRepositorySecret {
            version: REPOSITORY_SECRET_ENCRYPTION_VERSION,
            nonce: stored_nonce,
            ciphertext,
        })
    }

    /// Authenticates and decrypts a stored repository secret.
    ///
    /// `repository_name` must be the same canonical lowercase value used during encryption.
    ///
    /// # Errors
    ///
    /// Returns [`SecurityError::UnsupportedEncryptionVersion`] for unknown formats and
    /// [`SecurityError::DecryptionFailed`] for a wrong key or modified ciphertext, nonce, or
    /// associated repository name.
    pub fn decrypt(
        &self,
        repository_name: &CanonicalRepositoryName,
        encrypted: &EncryptedRepositorySecret,
    ) -> Result<RepositorySecret, SecurityError> {
        if encrypted.version != REPOSITORY_SECRET_ENCRYPTION_VERSION {
            return Err(SecurityError::UnsupportedEncryptionVersion {
                version: encrypted.version,
            });
        }

        let plaintext = Zeroizing::new(
            self.cipher
                .decrypt(
                    (&encrypted.nonce).into(),
                    Payload {
                        msg: &encrypted.ciphertext,
                        aad: repository_name.as_str().as_bytes(),
                    },
                )
                .map_err(|_| SecurityError::DecryptionFailed)?,
        );
        // This validates authenticated values produced outside this implementation and transfers
        // the allocation directly into zeroizing storage without copying the plaintext.
        RepositorySecret::from_decrypted_bytes(plaintext)
    }
}

impl fmt::Debug for RepositorySecretCipher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("RepositorySecretCipher([REDACTED])")
    }
}

fn derive_key(
    master_key: &MasterKey,
) -> Result<Zeroizing<[u8; DERIVED_KEY_LENGTH]>, SecurityError> {
    let mut derived_key = Zeroizing::new([0_u8; DERIVED_KEY_LENGTH]);
    Hkdf::<Sha256>::new(None, master_key.expose_secret())
        .expand(KEY_DERIVATION_CONTEXT, derived_key.as_mut())
        .map_err(|_| SecurityError::KeyDerivationFailed)?;
    Ok(derived_key)
}

#[cfg(test)]
mod tests {
    use super::{derive_key, RepositorySecretCipher};
    use crate::security::{
        CanonicalRepositoryName, EncryptedRepositorySecret, MasterKey, RepositorySecret,
        SecurityError,
    };

    fn cipher(key_byte: u8) -> RepositorySecretCipher {
        RepositorySecretCipher::new(
            &MasterKey::from_slice(&[key_byte; 32]).expect("test key is valid"),
        )
        .expect("key derivation succeeds")
    }

    fn repository_name(value: &str) -> CanonicalRepositoryName {
        CanonicalRepositoryName::new(value).expect("test repository name is valid")
    }

    fn repository_secret() -> RepositorySecret {
        RepositorySecret::new("webhook-secret".to_owned()).expect("test secret is valid")
    }

    #[test]
    fn derived_key_matches_known_hkdf_value() {
        let source_key: [u8; 32] = std::array::from_fn(|index| index as u8);
        let derived_key =
            derive_key(&MasterKey::from_slice(&source_key).expect("test key is valid"))
                .expect("key derivation succeeds");

        assert_eq!(
            derived_key.as_ref(),
            &[
                0x15, 0x92, 0xbf, 0x4e, 0x90, 0xc9, 0x0a, 0x81, 0x6a, 0x9d, 0xff, 0x4f, 0xbe, 0xca,
                0xe9, 0xfc, 0xae, 0xd1, 0x75, 0x19, 0x3c, 0x08, 0x62, 0xe7, 0xb3, 0xfb, 0xda, 0x6d,
                0xd0, 0xe9, 0x5e, 0xcf,
            ]
        );
    }

    #[test]
    fn encryption_round_trips() {
        let cipher = cipher(7);
        let name = repository_name("owner/repository");
        let encrypted = cipher
            .encrypt(&name, &repository_secret())
            .expect("encryption succeeds");

        let decrypted = cipher
            .decrypt(&name, &encrypted)
            .expect("decryption succeeds");

        assert_eq!(decrypted.expose_secret(), "webhook-secret");
    }

    #[test]
    fn repeated_encryption_uses_distinct_nonces_and_ciphertexts() {
        let cipher = cipher(7);
        let name = repository_name("owner/repository");
        let secret = repository_secret();

        let first = cipher.encrypt(&name, &secret).expect("encryption succeeds");
        let second = cipher.encrypt(&name, &secret).expect("encryption succeeds");

        assert_ne!(first.nonce(), second.nonce());
        assert_ne!(first.ciphertext(), second.ciphertext());
    }

    #[test]
    fn decryption_rejects_wrong_key_and_associated_data() {
        let encrypting_cipher = cipher(7);
        let name = repository_name("owner/repository");
        let encrypted = encrypting_cipher
            .encrypt(&name, &repository_secret())
            .expect("encryption succeeds");

        assert_eq!(
            cipher(8)
                .decrypt(&name, &encrypted)
                .expect_err("wrong keys must fail"),
            SecurityError::DecryptionFailed
        );
        assert_eq!(
            encrypting_cipher
                .decrypt(&repository_name("owner/other"), &encrypted)
                .expect_err("different associated data must fail"),
            SecurityError::DecryptionFailed
        );
    }

    #[test]
    fn decryption_rejects_modified_ciphertext_and_nonce() {
        let cipher = cipher(7);
        let name = repository_name("owner/repository");
        let encrypted = cipher
            .encrypt(&name, &repository_secret())
            .expect("encryption succeeds");
        let mut changed_ciphertext = encrypted.ciphertext().to_vec();
        changed_ciphertext[0] ^= 1;
        let changed_ciphertext = EncryptedRepositorySecret::from_parts(
            encrypted.version(),
            encrypted.nonce(),
            changed_ciphertext,
        )
        .expect("modified storage fields remain structurally valid");
        let mut changed_nonce = *encrypted.nonce();
        changed_nonce[0] ^= 1;
        let changed_nonce = EncryptedRepositorySecret::from_parts(
            encrypted.version(),
            &changed_nonce,
            encrypted.ciphertext().to_vec(),
        )
        .expect("modified storage fields remain structurally valid");

        assert_eq!(
            cipher
                .decrypt(&name, &changed_ciphertext)
                .expect_err("modified ciphertext must fail"),
            SecurityError::DecryptionFailed
        );
        assert_eq!(
            cipher
                .decrypt(&name, &changed_nonce)
                .expect_err("modified nonces must fail"),
            SecurityError::DecryptionFailed
        );
    }

    #[test]
    fn decryption_rejects_unsupported_versions() {
        let cipher = cipher(7);
        let encrypted = EncryptedRepositorySecret {
            version: 2,
            nonce: [0_u8; 12],
            ciphertext: vec![0_u8; 16],
        };

        assert_eq!(
            cipher
                .decrypt(&repository_name("owner/repository"), &encrypted)
                .expect_err("unsupported versions must fail closed"),
            SecurityError::UnsupportedEncryptionVersion { version: 2 }
        );
    }
}
