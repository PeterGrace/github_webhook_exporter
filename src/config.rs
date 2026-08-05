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
use secrecy::zeroize::Zeroizing;
use thiserror::Error;
use tracing_subscriber::EnvFilter;

use crate::security::{AdminToken, MasterKey};

const DEFAULT_BIND_ADDRESS: &str = "[::]:8080";
const DEFAULT_RUST_LOG: &str = "info";
const DEFAULT_SHUTDOWN_TIMEOUT_SECONDS: u64 = 30;
const DEFAULT_WEBHOOK_BODY_LIMIT_BYTES: u64 = 2_097_152;
const MAX_WEBHOOK_BODY_LIMIT_BYTES: u64 = 2_097_152;
const DEFAULT_DELIVERY_RETENTION_DAYS: u64 = 7;
const DEFAULT_MERGE_QUEUE_RETENTION_DAYS: u64 = 90;
const DEFAULT_DELIVERY_PRUNE_INTERVAL_SECONDS: u64 = 3_600;
const SECONDS_PER_DAY: u64 = 86_400;
const MASTER_KEY_LENGTH: usize = 32;

/// Fully validated process configuration loaded from environment variables.
pub struct RuntimeConfig {
    database_path: PathBuf,
    master_key: MasterKey,
    admin_token: AdminToken,
    bind_address: SocketAddr,
    shutdown_timeout: Duration,
    webhook_body_limit_bytes: usize,
    delivery_retention: Duration,
    merge_queue_retention: Duration,
    delivery_prune_interval: Duration,
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
    pub fn master_key(&self) -> &MasterKey {
        &self.master_key
    }

    /// Returns the zeroizing configuration API credential.
    pub fn admin_token(&self) -> &AdminToken {
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

    /// Returns the maximum accepted GitHub webhook request-body size in bytes.
    pub fn webhook_body_limit_bytes(&self) -> usize {
        self.webhook_body_limit_bytes
    }

    /// Returns how long processed webhook delivery identifiers are retained.
    pub fn delivery_retention(&self) -> Duration {
        self.delivery_retention
    }

    /// Returns how long completed merge-queue attempts are retained.
    pub fn merge_queue_retention(&self) -> Duration {
        self.merge_queue_retention
    }

    /// Returns the interval between retention pruning passes.
    pub fn delivery_prune_interval(&self) -> Duration {
        self.delivery_prune_interval
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
        let master_key = MasterKey::from_slice(decoded_master_key.as_slice()).map_err(|_| {
            ConfigError::Invalid {
                variable: "GHE_MASTER_KEY",
            }
        })?;

        let admin_token = AdminToken::new(required_string(&mut lookup, "GHE_ADMIN_TOKEN")?)
            .map_err(|_| ConfigError::Invalid {
                variable: "GHE_ADMIN_TOKEN",
            })?;

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

        let webhook_body_limit_bytes = optional_positive_u64(
            &mut lookup,
            "GHE_WEBHOOK_BODY_LIMIT_BYTES",
            DEFAULT_WEBHOOK_BODY_LIMIT_BYTES,
        )?;
        if webhook_body_limit_bytes > MAX_WEBHOOK_BODY_LIMIT_BYTES {
            return Err(ConfigError::Invalid {
                variable: "GHE_WEBHOOK_BODY_LIMIT_BYTES",
            });
        }
        let webhook_body_limit_bytes =
            usize::try_from(webhook_body_limit_bytes).map_err(|_| ConfigError::Invalid {
                variable: "GHE_WEBHOOK_BODY_LIMIT_BYTES",
            })?;
        let delivery_retention_days = optional_positive_u64(
            &mut lookup,
            "GHE_DELIVERY_RETENTION_DAYS",
            DEFAULT_DELIVERY_RETENTION_DAYS,
        )?;
        let delivery_retention_seconds = delivery_retention_days
            .checked_mul(SECONDS_PER_DAY)
            .ok_or(ConfigError::Invalid {
                variable: "GHE_DELIVERY_RETENTION_DAYS",
            })?;
        let merge_queue_retention_days = optional_positive_u64(
            &mut lookup,
            "GHE_MERGE_QUEUE_RETENTION_DAYS",
            DEFAULT_MERGE_QUEUE_RETENTION_DAYS,
        )?;
        let merge_queue_retention_seconds = merge_queue_retention_days
            .checked_mul(SECONDS_PER_DAY)
            .ok_or(ConfigError::Invalid {
                variable: "GHE_MERGE_QUEUE_RETENTION_DAYS",
            })?;
        let delivery_prune_interval_seconds = optional_positive_u64(
            &mut lookup,
            "GHE_DELIVERY_PRUNE_INTERVAL_SECONDS",
            DEFAULT_DELIVERY_PRUNE_INTERVAL_SECONDS,
        )?;

        let rust_log = optional_string(&mut lookup, "RUST_LOG")?
            .unwrap_or_else(|| DEFAULT_RUST_LOG.to_owned());
        EnvFilter::try_new(&rust_log).map_err(|_| ConfigError::Invalid {
            variable: "RUST_LOG",
        })?;

        Ok(Self {
            database_path: PathBuf::from(database_path),
            master_key,
            admin_token,
            bind_address,
            shutdown_timeout: Duration::from_secs(shutdown_timeout_seconds),
            webhook_body_limit_bytes,
            delivery_retention: Duration::from_secs(delivery_retention_seconds),
            merge_queue_retention: Duration::from_secs(merge_queue_retention_seconds),
            delivery_prune_interval: Duration::from_secs(delivery_prune_interval_seconds),
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
            .field("webhook_body_limit_bytes", &self.webhook_body_limit_bytes)
            .field("delivery_retention", &self.delivery_retention)
            .field("merge_queue_retention", &self.merge_queue_retention)
            .field("delivery_prune_interval", &self.delivery_prune_interval)
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

fn optional_positive_u64(
    lookup: &mut impl FnMut(&str) -> Option<OsString>,
    variable: &'static str,
    default: u64,
) -> Result<u64, ConfigError> {
    let value = optional_string(lookup, variable)?.map_or(Ok(default), |value| {
        value
            .parse::<u64>()
            .map_err(|_| ConfigError::Invalid { variable })
    })?;
    if value == 0 {
        return Err(ConfigError::Invalid { variable });
    }

    Ok(value)
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, ffi::OsString, net::SocketAddr, path::Path, time::Duration};

    use base64::{engine::general_purpose::STANDARD, Engine as _};

    use crate::security::{AdminAuthenticator, RepositorySecretCipher};

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
        assert!(RepositorySecretCipher::new(config.master_key()).is_ok());
        assert_eq!(
            AdminAuthenticator::new(config.admin_token())
                .authenticate(Some("Bearer admin-token-value")),
            Ok(())
        );
        assert_eq!(config.bind_address(), SocketAddr::from(([0_u16; 8], 8080)));
        assert_eq!(config.shutdown_timeout(), Duration::from_secs(30));
        assert_eq!(config.webhook_body_limit_bytes(), 2_097_152);
        assert_eq!(config.delivery_retention(), Duration::from_secs(7 * 86_400));
        assert_eq!(
            config.merge_queue_retention(),
            Duration::from_secs(90 * 86_400)
        );
        assert_eq!(config.delivery_prune_interval(), Duration::from_secs(3_600));
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
            "GHE_WEBHOOK_BODY_LIMIT_BYTES".to_owned(),
            OsString::from("2097152"),
        );
        variables.insert(
            "GHE_DELIVERY_RETENTION_DAYS".to_owned(),
            OsString::from("14"),
        );
        variables.insert(
            "GHE_MERGE_QUEUE_RETENTION_DAYS".to_owned(),
            OsString::from("180"),
        );
        variables.insert(
            "GHE_DELIVERY_PRUNE_INTERVAL_SECONDS".to_owned(),
            OsString::from("120"),
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
        assert_eq!(config.webhook_body_limit_bytes(), 2_097_152);
        assert_eq!(
            config.delivery_retention(),
            Duration::from_secs(14 * 86_400)
        );
        assert_eq!(
            config.merge_queue_retention(),
            Duration::from_secs(180 * 86_400)
        );
        assert_eq!(config.delivery_prune_interval(), Duration::from_secs(120));
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
            ("GHE_WEBHOOK_BODY_LIMIT_BYTES", "0"),
            ("GHE_WEBHOOK_BODY_LIMIT_BYTES", "not-a-number"),
            ("GHE_WEBHOOK_BODY_LIMIT_BYTES", "2097153"),
            ("GHE_DELIVERY_RETENTION_DAYS", "0"),
            ("GHE_DELIVERY_RETENTION_DAYS", "not-a-number"),
            ("GHE_DELIVERY_RETENTION_DAYS", "18446744073709551615"),
            ("GHE_MERGE_QUEUE_RETENTION_DAYS", "0"),
            ("GHE_MERGE_QUEUE_RETENTION_DAYS", "not-a-number"),
            ("GHE_MERGE_QUEUE_RETENTION_DAYS", "18446744073709551615"),
            ("GHE_DELIVERY_PRUNE_INTERVAL_SECONDS", "0"),
            ("GHE_DELIVERY_PRUNE_INTERVAL_SECONDS", "not-a-number"),
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

    #[cfg(unix)]
    #[test]
    fn merge_queue_retention_rejects_non_unicode_without_disclosure() {
        use std::os::unix::ffi::OsStringExt;

        let mut variables = required_variables();
        variables.insert(
            "GHE_MERGE_QUEUE_RETENTION_DAYS".to_owned(),
            OsString::from_vec(vec![0xff]),
        );

        let error = RuntimeConfig::from_map(variables).expect_err("value must be rejected");

        assert_eq!(
            error,
            ConfigError::Invalid {
                variable: "GHE_MERGE_QUEUE_RETENTION_DAYS",
            }
        );
        assert_eq!(
            error.to_string(),
            "environment variable GHE_MERGE_QUEUE_RETENTION_DAYS has an invalid value"
        );
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
