use std::{
    collections::HashMap,
    env,
    ffi::OsString,
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
    str::FromStr,
    time::Duration,
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use http::{HeaderName, HeaderValue};
use percent_encoding::percent_decode_str;
use secrecy::zeroize::Zeroizing;
use thiserror::Error;
use tracing_subscriber::EnvFilter;

use crate::security::{AdminToken, MasterKey};

const DEFAULT_BIND_ADDRESS: &str = "[::]:8080";
const DEFAULT_RUST_LOG: &str = "info";
const DEFAULT_SHUTDOWN_TIMEOUT_SECONDS: u64 = 30;
const DEFAULT_WEBHOOK_BODY_LIMIT_BYTES: u64 = 2_097_152;
const MAX_WEBHOOK_BODY_LIMIT_BYTES: u64 = 2_097_152;
/// Default maximum number of workflow-job steps reported per completed trace.
pub const DEFAULT_WORKFLOW_JOB_MAX_STEPS: usize = 256;
const MAX_WORKFLOW_JOB_MAX_STEPS: usize = 1_024;
const DEFAULT_DELIVERY_RETENTION_DAYS: u64 = 7;
const DEFAULT_MERGE_QUEUE_RETENTION_DAYS: u64 = 90;
const DEFAULT_DELIVERY_PRUNE_INTERVAL_SECONDS: u64 = 3_600;
const SECONDS_PER_DAY: u64 = 86_400;
const MASTER_KEY_LENGTH: usize = 32;
const DEFAULT_OTEL_SERVICE_NAME: &str = "github-webhook-exporter";
const DEFAULT_OTEL_EXPORT_TIMEOUT_MILLISECONDS: u64 = 10_000;
pub(crate) const DEFAULT_OTEL_QUEUE_CAPACITY: usize = 2_048;
pub(crate) const DEFAULT_OTEL_BATCH_SIZE: usize = 512;
const DEFAULT_OTEL_SHUTDOWN_TIMEOUT_SECONDS: u64 = 5;

type SensitiveHeaders = Vec<(String, Zeroizing<String>)>;

/// Validated, redacted configuration for optional OTLP trace and log export.
pub struct TelemetryConfig {
    pub(crate) trace_exporter: Option<ExporterSettings>,
    pub(crate) log_exporter: Option<ExporterSettings>,
    queue_capacity: usize,
    batch_size: usize,
    shutdown_timeout: Duration,
    service_name: String,
    sentry_dsn: Option<Zeroizing<String>>,
    pub(crate) resource_attributes: Vec<(String, String)>,
}

impl TelemetryConfig {
    /// Returns whether at least one remote OTLP signal is enabled.
    pub fn is_enabled(&self) -> bool {
        self.trace_exporter.is_some() || self.log_exporter.is_some()
    }

    /// Returns the maximum number of admitted records per enabled signal.
    pub fn queue_capacity(&self) -> usize {
        self.queue_capacity
    }

    /// Returns the maximum number of records in one export request.
    pub fn batch_size(&self) -> usize {
        self.batch_size
    }

    /// Returns the shared maximum telemetry shutdown duration.
    pub fn shutdown_timeout(&self) -> Duration {
        self.shutdown_timeout
    }

    /// Returns the configured OpenTelemetry service name.
    pub fn service_name(&self) -> &str {
        &self.service_name
    }

    /// Returns the optional Sentry DSN used for linked workflow-task errors.
    pub(crate) fn sentry_dsn(&self) -> Option<&str> {
        self.sentry_dsn.as_deref().map(String::as_str)
    }

    /// Returns the approved Kubernetes pod resource attribute, when configured.
    pub fn pod_name(&self) -> Option<&str> {
        self.resource_attribute("k8s.pod.name")
    }

    /// Returns the approved Kubernetes namespace resource attribute, when configured.
    pub fn kubernetes_namespace(&self) -> Option<&str> {
        self.resource_attribute("k8s.namespace.name")
    }

    fn resource_attribute(&self, key: &str) -> Option<&str> {
        self.resource_attributes
            .iter()
            .find_map(|(candidate, value)| (candidate == key).then_some(value.as_str()))
    }
}

impl fmt::Debug for TelemetryConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TelemetryConfig")
            .field("enabled", &self.is_enabled())
            .field("transport", &"[REDACTED]")
            .field("queue_capacity", &self.queue_capacity)
            .field("batch_size", &self.batch_size)
            .field("shutdown_timeout", &self.shutdown_timeout)
            .field("service_name", &self.service_name)
            .field(
                "sentry_dsn",
                &self.sentry_dsn.as_ref().map(|_| "[REDACTED]"),
            )
            .field("resource_attributes", &self.resource_attributes)
            .finish()
    }
}

pub(crate) struct ExporterSettings {
    endpoint: Zeroizing<String>,
    headers: SensitiveHeaders,
    pub(crate) timeout: Duration,
}

impl ExporterSettings {
    pub(crate) fn endpoint(&self) -> &str {
        self.endpoint.as_str()
    }

    pub(crate) fn headers(&self) -> HashMap<String, String> {
        self.headers
            .iter()
            .map(|(name, value)| (name.clone(), value.to_string()))
            .collect()
    }
}

impl fmt::Debug for ExporterSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("ExporterSettings([REDACTED])")
    }
}

/// Fully validated process configuration loaded from environment variables.
pub struct RuntimeConfig {
    database_path: PathBuf,
    master_key: MasterKey,
    admin_token: AdminToken,
    bind_address: SocketAddr,
    shutdown_timeout: Duration,
    webhook_body_limit_bytes: usize,
    workflow_job_max_steps: usize,
    delivery_retention: Duration,
    merge_queue_retention: Duration,
    delivery_prune_interval: Duration,
    rust_log: String,
    telemetry: TelemetryConfig,
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

    /// Returns the maximum reported steps accepted for one completed workflow-job trace.
    pub fn workflow_job_max_steps(&self) -> usize {
        self.workflow_job_max_steps
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

    /// Returns the validated optional remote telemetry configuration.
    pub fn telemetry(&self) -> &TelemetryConfig {
        &self.telemetry
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
        let workflow_job_max_steps = optional_positive_usize(
            &mut lookup,
            "GHE_WORKFLOW_JOB_MAX_STEPS",
            DEFAULT_WORKFLOW_JOB_MAX_STEPS,
        )?;
        if workflow_job_max_steps > MAX_WORKFLOW_JOB_MAX_STEPS {
            return Err(ConfigError::Invalid {
                variable: "GHE_WORKFLOW_JOB_MAX_STEPS",
            });
        }

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

        let telemetry = TelemetryConfig::from_lookup(&mut lookup)?;

        Ok(Self {
            database_path: PathBuf::from(database_path),
            master_key,
            admin_token,
            bind_address,
            shutdown_timeout: Duration::from_secs(shutdown_timeout_seconds),
            webhook_body_limit_bytes,
            workflow_job_max_steps,
            delivery_retention: Duration::from_secs(delivery_retention_seconds),
            merge_queue_retention: Duration::from_secs(merge_queue_retention_seconds),
            delivery_prune_interval: Duration::from_secs(delivery_prune_interval_seconds),
            rust_log,
            telemetry,
        })
    }

    #[cfg(test)]
    fn from_map(variables: HashMap<String, OsString>) -> Result<Self, ConfigError> {
        Self::from_lookup(|variable| variables.get(variable).cloned())
    }
}

impl TelemetryConfig {
    pub(crate) fn from_lookup(
        lookup: &mut impl FnMut(&str) -> Option<OsString>,
    ) -> Result<Self, ConfigError> {
        let queue_capacity = optional_positive_usize(
            lookup,
            "GHE_OTEL_QUEUE_CAPACITY",
            DEFAULT_OTEL_QUEUE_CAPACITY,
        )?;
        let batch_size =
            optional_positive_usize(lookup, "GHE_OTEL_BATCH_SIZE", DEFAULT_OTEL_BATCH_SIZE)?;
        if batch_size > queue_capacity {
            return Err(ConfigError::Invalid {
                variable: "GHE_OTEL_BATCH_SIZE",
            });
        }
        let shutdown_timeout_seconds = optional_positive_u64(
            lookup,
            "GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS",
            DEFAULT_OTEL_SHUTDOWN_TIMEOUT_SECONDS,
        )?;

        let generic_endpoint = validated_endpoint(lookup, "OTEL_EXPORTER_OTLP_ENDPOINT")?;
        let trace_endpoint = validated_endpoint(lookup, "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")?
            .or_else(|| append_signal_path(generic_endpoint.as_deref(), "v1/traces"));
        let log_endpoint = validated_endpoint(lookup, "OTEL_EXPORTER_OTLP_LOGS_ENDPOINT")?
            .or_else(|| append_signal_path(generic_endpoint.as_deref(), "v1/logs"));

        let generic_headers = validated_headers(lookup, "OTEL_EXPORTER_OTLP_HEADERS")?;
        // An explicitly empty signal value clears inherited generic headers;
        // only absence falls back.
        let trace_headers = validated_headers(lookup, "OTEL_EXPORTER_OTLP_TRACES_HEADERS")?
            .unwrap_or_else(|| clone_headers(&generic_headers));
        let log_headers = validated_headers(lookup, "OTEL_EXPORTER_OTLP_LOGS_HEADERS")?
            .unwrap_or_else(|| clone_headers(&generic_headers));

        let generic_timeout = optional_positive_u64(
            lookup,
            "OTEL_EXPORTER_OTLP_TIMEOUT",
            DEFAULT_OTEL_EXPORT_TIMEOUT_MILLISECONDS,
        )?;
        let trace_timeout =
            optional_positive_u64(lookup, "OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", generic_timeout)?;
        let log_timeout =
            optional_positive_u64(lookup, "OTEL_EXPORTER_OTLP_LOGS_TIMEOUT", generic_timeout)?;

        let service_name = optional_string(lookup, "OTEL_SERVICE_NAME")?
            .unwrap_or_else(|| DEFAULT_OTEL_SERVICE_NAME.to_owned());
        if service_name.is_empty() {
            return Err(ConfigError::Invalid {
                variable: "OTEL_SERVICE_NAME",
            });
        }
        let sentry_dsn = optional_string(lookup, "SENTRY_DSN")?
            .map(|dsn| {
                dsn.parse::<sentry::types::Dsn>()
                    .map(|_| Zeroizing::new(dsn))
                    .map_err(|_| ConfigError::Invalid {
                        variable: "SENTRY_DSN",
                    })
            })
            .transpose()?;
        if sentry_dsn.is_some() && trace_endpoint.is_none() {
            return Err(ConfigError::Invalid {
                variable: "SENTRY_DSN",
            });
        }
        let resource_attributes = validated_resource_attributes(lookup)?;

        Ok(Self {
            trace_exporter: trace_endpoint.map(|endpoint| ExporterSettings {
                endpoint: Zeroizing::new(endpoint),
                headers: trace_headers,
                timeout: Duration::from_millis(trace_timeout),
            }),
            log_exporter: log_endpoint.map(|endpoint| ExporterSettings {
                endpoint: Zeroizing::new(endpoint),
                headers: log_headers,
                timeout: Duration::from_millis(log_timeout),
            }),
            queue_capacity,
            batch_size,
            shutdown_timeout: Duration::from_secs(shutdown_timeout_seconds),
            service_name,
            sentry_dsn,
            resource_attributes,
        })
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
            .field("workflow_job_max_steps", &self.workflow_job_max_steps)
            .field("delivery_retention", &self.delivery_retention)
            .field("merge_queue_retention", &self.merge_queue_retention)
            .field("delivery_prune_interval", &self.delivery_prune_interval)
            .field("rust_log", &self.rust_log)
            .field("telemetry", &self.telemetry)
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

fn optional_positive_usize(
    lookup: &mut impl FnMut(&str) -> Option<OsString>,
    variable: &'static str,
    default: usize,
) -> Result<usize, ConfigError> {
    let value = optional_string(lookup, variable)?.map_or(Ok(default), |value| {
        value
            .parse::<usize>()
            .map_err(|_| ConfigError::Invalid { variable })
    })?;
    if value == 0 {
        return Err(ConfigError::Invalid { variable });
    }

    Ok(value)
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

fn validated_endpoint(
    lookup: &mut impl FnMut(&str) -> Option<OsString>,
    variable: &'static str,
) -> Result<Option<String>, ConfigError> {
    optional_string(lookup, variable)?
        .map(|endpoint| {
            let url = url::Url::parse(&endpoint).map_err(|_| ConfigError::Invalid { variable })?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(ConfigError::Invalid { variable });
            }
            Ok(endpoint)
        })
        .transpose()
}

fn append_signal_path(endpoint: Option<&str>, signal_path: &str) -> Option<String> {
    endpoint.map(|endpoint| format!("{}/{signal_path}", endpoint.trim_end_matches('/')))
}

fn validated_headers(
    lookup: &mut impl FnMut(&str) -> Option<OsString>,
    variable: &'static str,
) -> Result<Option<SensitiveHeaders>, ConfigError> {
    optional_string(lookup, variable)?
        .map(|headers| {
            if headers.is_empty() {
                return Ok(Vec::new());
            }
            headers
                .split(',')
                .map(str::trim)
                .map(|header| {
                    let (name, encoded_value) = header
                        .split_once('=')
                        .ok_or(ConfigError::Invalid { variable })?;
                    let name = name.trim();
                    let value = percent_decode_str(encoded_value.trim())
                        .decode_utf8()
                        .map_err(|_| ConfigError::Invalid { variable })?
                        .into_owned();
                    if name.is_empty() || value.is_empty() {
                        return Err(ConfigError::Invalid { variable });
                    }
                    HeaderName::from_str(name).map_err(|_| ConfigError::Invalid { variable })?;
                    HeaderValue::from_str(&value).map_err(|_| ConfigError::Invalid { variable })?;
                    Ok((name.to_owned(), Zeroizing::new(value)))
                })
                .collect()
        })
        .transpose()
}

fn clone_headers(headers: &Option<SensitiveHeaders>) -> SensitiveHeaders {
    headers
        .as_ref()
        .map(|headers| {
            headers
                .iter()
                .map(|(name, value)| (name.clone(), Zeroizing::new(value.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

fn validated_resource_attributes(
    lookup: &mut impl FnMut(&str) -> Option<OsString>,
) -> Result<Vec<(String, String)>, ConfigError> {
    const VARIABLE: &str = "OTEL_RESOURCE_ATTRIBUTES";
    let Some(attributes) = optional_string(lookup, VARIABLE)? else {
        return Ok(Vec::new());
    };
    if attributes.is_empty() {
        return Ok(Vec::new());
    }

    attributes
        .split(',')
        .filter_map(|attribute| {
            let (encoded_key, encoded_value) = match attribute.split_once('=') {
                Some(parts) => parts,
                None => return Some(Err(ConfigError::Invalid { variable: VARIABLE })),
            };
            let key = match percent_decode_str(encoded_key).decode_utf8() {
                Ok(key) => key,
                Err(_) => return Some(Err(ConfigError::Invalid { variable: VARIABLE })),
            };
            if !matches!(key.as_ref(), "k8s.pod.name" | "k8s.namespace.name") {
                return None;
            }
            let value = match percent_decode_str(encoded_value).decode_utf8() {
                Ok(value) if !value.is_empty() => value.into_owned(),
                Ok(_) | Err(_) => {
                    return Some(Err(ConfigError::Invalid { variable: VARIABLE }));
                }
            };
            Some(Ok((key.into_owned(), value)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, ffi::OsString, net::SocketAddr, path::Path, time::Duration};

    use base64::{engine::general_purpose::STANDARD, Engine as _};

    use crate::security::{AdminAuthenticator, RepositorySecretCipher};

    use super::{ConfigError, RuntimeConfig, DEFAULT_OTEL_BATCH_SIZE, DEFAULT_OTEL_QUEUE_CAPACITY};

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
        assert_eq!(config.workflow_job_max_steps(), 256);
        assert_eq!(config.rust_log(), "info");
    }

    #[test]
    fn valid_minimum_workflow_job_max_steps_is_accepted() {
        let mut variables = required_variables();
        variables.insert("GHE_WORKFLOW_JOB_MAX_STEPS".to_owned(), OsString::from("1"));

        let config = RuntimeConfig::from_map(variables).expect("minimum step limit is valid");

        assert_eq!(config.workflow_job_max_steps(), 1);
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
            "GHE_WORKFLOW_JOB_MAX_STEPS".to_owned(),
            OsString::from("1024"),
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
        assert_eq!(config.workflow_job_max_steps(), 1_024);
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
            ("GHE_WORKFLOW_JOB_MAX_STEPS", "0"),
            ("GHE_WORKFLOW_JOB_MAX_STEPS", "not-a-number"),
            ("GHE_WORKFLOW_JOB_MAX_STEPS", "1025"),
            ("GHE_WORKFLOW_JOB_MAX_STEPS", "18446744073709551616"),
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

    #[cfg(unix)]
    #[test]
    fn workflow_job_max_steps_rejects_non_unicode_without_disclosure() {
        use std::os::unix::ffi::OsStringExt;

        let mut variables = required_variables();
        variables.insert(
            "GHE_WORKFLOW_JOB_MAX_STEPS".to_owned(),
            OsString::from_vec(vec![0xff]),
        );

        let error = RuntimeConfig::from_map(variables).expect_err("value must be rejected");

        assert_eq!(
            error,
            ConfigError::Invalid {
                variable: "GHE_WORKFLOW_JOB_MAX_STEPS",
            }
        );
        assert_eq!(
            error.to_string(),
            "environment variable GHE_WORKFLOW_JOB_MAX_STEPS has an invalid value"
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

    #[test]
    fn telemetry_defaults_to_disabled_with_bounded_settings() {
        let config = RuntimeConfig::from_map(required_variables()).expect("configuration is valid");
        let telemetry = config.telemetry();

        assert!(!telemetry.is_enabled());
        assert_eq!(telemetry.queue_capacity(), DEFAULT_OTEL_QUEUE_CAPACITY);
        assert_eq!(telemetry.batch_size(), DEFAULT_OTEL_BATCH_SIZE);
        assert_eq!(telemetry.shutdown_timeout(), Duration::from_secs(5));
        assert_eq!(telemetry.service_name(), "github-webhook-exporter");
        assert_eq!(telemetry.sentry_dsn(), None);
        assert_eq!(telemetry.pod_name(), None);
        assert_eq!(telemetry.kubernetes_namespace(), None);
    }

    #[test]
    fn telemetry_valid_overrides_resolve_signal_specific_settings() {
        let mut variables = required_variables();
        for (variable, value) in [
            (
                "OTEL_EXPORTER_OTLP_ENDPOINT",
                "https://collector.example/base",
            ),
            (
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT",
                "https://traces.example/v1/traces",
            ),
            (
                "OTEL_EXPORTER_OTLP_HEADERS",
                " authorization = generic-secret ",
            ),
            ("OTEL_EXPORTER_OTLP_LOGS_HEADERS", "x-api-key=signal-secret"),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "1200"),
            ("OTEL_EXPORTER_OTLP_TRACES_TIMEOUT", "800"),
            ("OTEL_SERVICE_NAME", "custom-exporter"),
            ("SENTRY_DSN", "https://public@example.ingest.sentry.io/42"),
            (
                "OTEL_RESOURCE_ATTRIBUTES",
                "k8s.pod.name=exporter-0,k8s.namespace.name=observability,forbidden=value",
            ),
            ("GHE_OTEL_QUEUE_CAPACITY", "16"),
            ("GHE_OTEL_BATCH_SIZE", "4"),
            ("GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS", "9"),
        ] {
            variables.insert(variable.to_owned(), OsString::from(value));
        }

        let config = RuntimeConfig::from_map(variables).expect("configuration is valid");
        let telemetry = config.telemetry();

        assert!(telemetry.is_enabled());
        assert_eq!(telemetry.queue_capacity(), 16);
        assert_eq!(telemetry.batch_size(), 4);
        assert_eq!(telemetry.shutdown_timeout(), Duration::from_secs(9));
        assert_eq!(telemetry.service_name(), "custom-exporter");
        assert_eq!(
            telemetry.sentry_dsn(),
            Some("https://public@example.ingest.sentry.io/42")
        );
        assert_eq!(telemetry.pod_name(), Some("exporter-0"));
        assert_eq!(telemetry.kubernetes_namespace(), Some("observability"));
        let trace_exporter = telemetry
            .trace_exporter
            .as_ref()
            .expect("trace export is enabled");
        let log_exporter = telemetry
            .log_exporter
            .as_ref()
            .expect("log export is enabled");
        assert_eq!(
            trace_exporter.endpoint(),
            "https://traces.example/v1/traces"
        );
        assert_eq!(
            log_exporter.endpoint(),
            "https://collector.example/base/v1/logs"
        );
        assert_eq!(trace_exporter.timeout, Duration::from_millis(800));
        assert_eq!(log_exporter.timeout, Duration::from_millis(1200));
        assert!(trace_exporter.headers().contains_key("authorization"));
        assert!(log_exporter.headers().contains_key("x-api-key"));
        assert_eq!(telemetry.resource_attributes.len(), 2);
    }

    #[test]
    fn telemetry_rejects_invalid_or_inconsistent_settings_without_values() {
        let invalid_cases = [
            ("GHE_OTEL_QUEUE_CAPACITY", "0"),
            ("GHE_OTEL_QUEUE_CAPACITY", "not-a-number"),
            ("GHE_OTEL_QUEUE_CAPACITY", "18446744073709551616"),
            ("GHE_OTEL_BATCH_SIZE", "0"),
            ("GHE_OTEL_BATCH_SIZE", "17"),
            ("GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS", "0"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "not a URL"),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "ftp://collector.example"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "missing-equals"),
            ("OTEL_EXPORTER_OTLP_HEADERS", "bad header=value"),
            ("OTEL_EXPORTER_OTLP_TIMEOUT", "0"),
            ("OTEL_SERVICE_NAME", ""),
        ];

        for (variable, invalid_value) in invalid_cases {
            let mut variables = required_variables();
            variables.insert("GHE_OTEL_QUEUE_CAPACITY".to_owned(), OsString::from("16"));
            variables.insert("GHE_OTEL_BATCH_SIZE".to_owned(), OsString::from("4"));
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
    fn sentry_dsn_requires_a_valid_dsn_and_trace_export() {
        for invalid_dsn in ["not-a-dsn", "ftp://public@example.test/42"] {
            let mut variables = required_variables();
            variables.insert(
                "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT".to_owned(),
                OsString::from("https://traces.example/v1/traces"),
            );
            variables.insert("SENTRY_DSN".to_owned(), OsString::from(invalid_dsn));

            assert_eq!(
                RuntimeConfig::from_map(variables).expect_err("DSN must be rejected"),
                ConfigError::Invalid {
                    variable: "SENTRY_DSN"
                }
            );
        }

        let mut variables = required_variables();
        variables.insert(
            "SENTRY_DSN".to_owned(),
            OsString::from("https://public@example.ingest.sentry.io/42"),
        );
        assert_eq!(
            RuntimeConfig::from_map(variables).expect_err("linked errors require trace export"),
            ConfigError::Invalid {
                variable: "SENTRY_DSN"
            }
        );
    }

    #[cfg(unix)]
    #[test]
    fn telemetry_rejects_non_unicode_standard_settings() {
        use std::os::unix::ffi::OsStringExt;

        for variable in [
            "OTEL_EXPORTER_OTLP_ENDPOINT",
            "OTEL_EXPORTER_OTLP_HEADERS",
            "OTEL_SERVICE_NAME",
            "OTEL_RESOURCE_ATTRIBUTES",
            "SENTRY_DSN",
        ] {
            let mut variables = required_variables();
            variables.insert(variable.to_owned(), OsString::from_vec(vec![0xff]));

            assert_eq!(
                RuntimeConfig::from_map(variables).expect_err("value must be rejected"),
                ConfigError::Invalid { variable }
            );
        }
    }

    #[test]
    fn telemetry_debug_output_redacts_transport_configuration() {
        let mut variables = required_variables();
        variables.insert(
            "OTEL_EXPORTER_OTLP_ENDPOINT".to_owned(),
            OsString::from("https://credential@collector.example/private"),
        );
        variables.insert(
            "OTEL_EXPORTER_OTLP_HEADERS".to_owned(),
            OsString::from("authorization=secret-credential"),
        );
        variables.insert(
            "SENTRY_DSN".to_owned(),
            OsString::from("https://public:secret@example.ingest.sentry.io/42"),
        );

        let config = RuntimeConfig::from_map(variables).expect("configuration is valid");
        let rendered = format!("{config:?}");

        assert!(!rendered.contains("collector.example"));
        assert!(!rendered.contains("credential"));
        assert!(!rendered.contains("example.ingest.sentry.io"));
        assert!(!rendered.contains("public:secret"));
        assert!(rendered.contains("[REDACTED]"));
    }
}
