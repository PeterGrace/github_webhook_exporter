use std::{
    env,
    ffi::OsString,
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(test)]
use std::collections::HashMap;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use secrecy::{zeroize::Zeroizing, SecretBox, SecretString};
use thiserror::Error;
use tracing_subscriber::EnvFilter;

const DEFAULT_BIND_ADDRESS: &str = "[::]:8080";
const DEFAULT_RUST_LOG: &str = "info";
const DEFAULT_SHUTDOWN_TIMEOUT_SECONDS: u64 = 30;
const MASTER_KEY_LENGTH: usize = 32;

/// Fully validated process configuration loaded from environment variables.
pub struct RuntimeConfig {
    database_path: PathBuf,
    master_key: SecretBox<[u8; MASTER_KEY_LENGTH]>,
    admin_token: SecretString,
    bind_address: SocketAddr,
    shutdown_timeout: Duration,
    rust_log: String,
}

impl RuntimeConfig {
    /// Loads and validates process configuration from the current environment.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when a required variable is absent or any value is invalid. Error
    /// messages identify variables but never include their values.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::from_lookup(|variable| env::var_os(variable))
    }

    /// Returns the configured SQLite database path.
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    /// Returns the zeroizing database-encryption root key.
    pub fn master_key(&self) -> &SecretBox<[u8; MASTER_KEY_LENGTH]> {
        &self.master_key
    }

    /// Returns the zeroizing configuration API credential.
    pub fn admin_token(&self) -> &SecretString {
        &self.admin_token
    }

    /// Returns the address on which the HTTP server listens.
    pub fn bind_address(&self) -> SocketAddr {
        self.bind_address
    }

    /// Returns the maximum graceful-shutdown duration.
    pub fn shutdown_timeout(&self) -> Duration {
        self.shutdown_timeout
    }

    /// Returns the validated tracing filter directive.
    pub fn rust_log(&self) -> &str {
        &self.rust_log
    }

    /// Loads configuration from a caller-supplied environment lookup.
    pub(crate) fn from_lookup(
        mut lookup: impl FnMut(&str) -> Option<OsString>,
    ) -> Result<Self, ConfigError> {
        let database_path = required_os_string(&mut lookup, "GHE_DATABASE_PATH")?;
        if database_path.is_empty() {
            return Err(ConfigError::Invalid {
                variable: "GHE_DATABASE_PATH",
            });
        }

        let encoded_master_key = Zeroizing::new(required_string(&mut lookup, "GHE_MASTER_KEY")?);
        let decoded_master_key = Zeroizing::new(
            STANDARD
                .decode(encoded_master_key.as_bytes())
                .map_err(|_| ConfigError::Invalid {
                    variable: "GHE_MASTER_KEY",
                })?,
        );
        if decoded_master_key.len() != MASTER_KEY_LENGTH {
            return Err(ConfigError::Invalid {
                variable: "GHE_MASTER_KEY",
            });
        }
        let master_key = SecretBox::init_with_mut(|key: &mut [u8; MASTER_KEY_LENGTH]| {
            key.copy_from_slice(decoded_master_key.as_slice());
        });

        let admin_token = required_string(&mut lookup, "GHE_ADMIN_TOKEN")?;
        if admin_token.is_empty() {
            return Err(ConfigError::Invalid {
                variable: "GHE_ADMIN_TOKEN",
            });
        }

        let bind_address = optional_string(&mut lookup, "GHE_BIND_ADDRESS")?
            .unwrap_or_else(|| DEFAULT_BIND_ADDRESS.to_owned())
            .parse()
            .map_err(|_| ConfigError::Invalid {
                variable: "GHE_BIND_ADDRESS",
            })?;

        let shutdown_timeout_seconds = optional_string(
            &mut lookup,
            "GHE_SHUTDOWN_TIMEOUT_SECONDS",
        )?
        .map_or(Ok(DEFAULT_SHUTDOWN_TIMEOUT_SECONDS), |value| {
            value.parse::<u64>().map_err(|_| ConfigError::Invalid {
                variable: "GHE_SHUTDOWN_TIMEOUT_SECONDS",
            })
        })?;
        if shutdown_timeout_seconds == 0 {
            return Err(ConfigError::Invalid {
                variable: "GHE_SHUTDOWN_TIMEOUT_SECONDS",
            });
        }

        let rust_log = optional_string(&mut lookup, "RUST_LOG")?
            .unwrap_or_else(|| DEFAULT_RUST_LOG.to_owned());
        EnvFilter::try_new(&rust_log).map_err(|_| ConfigError::Invalid {
            variable: "RUST_LOG",
        })?;

        Ok(Self {
            database_path: PathBuf::from(database_path),
            master_key,
            admin_token: SecretString::from(admin_token),
            bind_address,
            shutdown_timeout: Duration::from_secs(shutdown_timeout_seconds),
            rust_log,
        })
    }

    #[cfg(test)]
    fn from_map(variables: HashMap<String, OsString>) -> Result<Self, ConfigError> {
        Self::from_lookup(|variable| variables.get(variable).cloned())
    }
}

impl fmt::Debug for RuntimeConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RuntimeConfig")
            .field("database_path", &self.database_path)
            .field("master_key", &"[REDACTED]")
            .field("admin_token", &"[REDACTED]")
            .field("bind_address", &self.bind_address)
            .field("shutdown_timeout", &self.shutdown_timeout)
            .field("rust_log", &self.rust_log)
            .finish()
    }
}

/// A redacted runtime-configuration failure.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    /// A required environment variable was not supplied.
    #[error("required environment variable {variable} is missing")]
    Missing {
        /// Name of the missing variable.
        variable: &'static str,
    },
    /// An environment variable did not satisfy its documented contract.
    #[error("environment variable {variable} has an invalid value")]
    Invalid {
        /// Name of the invalid variable.
        variable: &'static str,
    },
}

fn required_os_string(
    lookup: &mut impl FnMut(&str) -> Option<OsString>,
    variable: &'static str,
) -> Result<OsString, ConfigError> {
    lookup(variable).ok_or(ConfigError::Missing { variable })
}

fn required_string(
    lookup: &mut impl FnMut(&str) -> Option<OsString>,
    variable: &'static str,
) -> Result<String, ConfigError> {
    required_os_string(lookup, variable)?
        .into_string()
        .map_err(|_| ConfigError::Invalid { variable })
}

fn optional_string(
    lookup: &mut impl FnMut(&str) -> Option<OsString>,
    variable: &'static str,
) -> Result<Option<String>, ConfigError> {
    lookup(variable)
        .map(|value| {
            value
                .into_string()
                .map_err(|_| ConfigError::Invalid { variable })
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, ffi::OsString, net::SocketAddr, path::Path, time::Duration};

    use base64::{engine::general_purpose::STANDARD, Engine as _};
    use secrecy::ExposeSecret;

    use super::{ConfigError, RuntimeConfig};

    const ADMIN_TOKEN: &str = "admin-token-value";

    fn required_variables() -> HashMap<String, OsString> {
        HashMap::from([
            (
                "GHE_DATABASE_PATH".to_owned(),
                OsString::from("/tmp/exporter.db"),
            ),
            (
                "GHE_MASTER_KEY".to_owned(),
                OsString::from(STANDARD.encode([7_u8; 32])),
            ),
            ("GHE_ADMIN_TOKEN".to_owned(), OsString::from(ADMIN_TOKEN)),
        ])
    }

    #[test]
    fn valid_required_variables_use_documented_defaults() {
        let config = RuntimeConfig::from_map(required_variables()).expect("configuration is valid");

        assert_eq!(config.database_path(), Path::new("/tmp/exporter.db"));
        assert_eq!(config.master_key().expose_secret(), &[7_u8; 32]);
        assert_eq!(config.admin_token().expose_secret(), ADMIN_TOKEN);
        assert_eq!(config.bind_address(), SocketAddr::from(([0_u16; 8], 8080)));
        assert_eq!(config.shutdown_timeout(), Duration::from_secs(30));
        assert_eq!(config.rust_log(), "info");
    }

    #[test]
    fn valid_overrides_replace_defaults() {
        let mut variables = required_variables();
        variables.insert(
            "GHE_BIND_ADDRESS".to_owned(),
            OsString::from("127.0.0.1:9000"),
        );
        variables.insert(
            "GHE_SHUTDOWN_TIMEOUT_SECONDS".to_owned(),
            OsString::from("45"),
        );
        variables.insert(
            "RUST_LOG".to_owned(),
            OsString::from("github_webhook_exporter=debug"),
        );

        let config = RuntimeConfig::from_map(variables).expect("configuration is valid");

        assert_eq!(
            config.bind_address(),
            SocketAddr::from(([127, 0, 0, 1], 9000))
        );
        assert_eq!(config.shutdown_timeout(), Duration::from_secs(45));
        assert_eq!(config.rust_log(), "github_webhook_exporter=debug");
    }

    #[test]
    fn missing_required_variable_names_the_variable() {
        for variable in ["GHE_DATABASE_PATH", "GHE_MASTER_KEY", "GHE_ADMIN_TOKEN"] {
            let mut variables = required_variables();
            variables.remove(variable);

            let error = RuntimeConfig::from_map(variables).expect_err("variable must be required");

            assert_eq!(error, ConfigError::Missing { variable });
        }
    }

    #[test]
    fn invalid_values_report_only_variable_names() {
        let invalid_cases = [
            ("GHE_DATABASE_PATH", ""),
            ("GHE_MASTER_KEY", "not-base64"),
            ("GHE_MASTER_KEY", "c2hvcnQ="),
            ("GHE_ADMIN_TOKEN", ""),
            ("GHE_BIND_ADDRESS", "invalid-address"),
            ("GHE_SHUTDOWN_TIMEOUT_SECONDS", "0"),
            ("GHE_SHUTDOWN_TIMEOUT_SECONDS", "not-a-number"),
            ("RUST_LOG", "[invalid"),
        ];

        for (variable, invalid_value) in invalid_cases {
            let mut variables = required_variables();
            variables.insert(variable.to_owned(), OsString::from(invalid_value));

            let error = RuntimeConfig::from_map(variables).expect_err("value must be rejected");
            let rendered = error.to_string();

            assert!(rendered.contains(variable));
            if !invalid_value.is_empty() {
                assert!(!rendered.contains(invalid_value));
            }
        }
    }

    #[test]
    fn debug_output_redacts_all_credentials() {
        let config = RuntimeConfig::from_map(required_variables()).expect("configuration is valid");

        let rendered = format!("{config:?}");

        assert!(!rendered.contains(ADMIN_TOKEN));
        assert!(!rendered.contains(&STANDARD.encode([7_u8; 32])));
        assert!(rendered.contains("[REDACTED]"));
    }
}
