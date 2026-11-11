//! Optional, validated unit-price table for CI cost attribution.
//!
//! The table arrives as one JSON document in `GHE_COSTS_JSON`, rendered by the Helm chart from
//! `.Values.costs`. When the variable is unset every cost feature is off. When it is set but
//! invalid, startup fails with a [`ConfigError`](crate::config::ConfigError) that names the
//! variable and never its value.

use std::{
    collections::{BTreeMap, HashMap},
    fmt,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tracing::warn;

use crate::config::ConfigError;

/// Environment variable carrying the JSON price table.
pub const COSTS_VARIABLE: &str = "GHE_COSTS_JSON";
/// Number of hex characters kept from the SHA-256 digest of the canonical table.
pub const PRICE_TABLE_VERSION_LENGTH: usize = 12;
/// Maximum number of job labels included in one `ci price missing` warning.
pub const MAX_LOGGED_LABELS: usize = 8;
/// Maximum characters kept from each job label in one `ci price missing` warning.
pub const MAX_LOGGED_LABEL_CHARS: usize = 64;

/// A non-negative, finite USD amount for one billing unit.
///
/// `#[serde(try_from = "f64")]` routes every deserialized number through [`TryFrom`], so a
/// negative rate cannot be represented once parsing succeeds.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "f64", into = "f64")]
pub struct UsdRate(f64);

impl UsdRate {
    /// Returns the rate in US dollars.
    pub fn usd(self) -> f64 {
        self.0
    }
}

impl TryFrom<f64> for UsdRate {
    type Error = &'static str;

    fn try_from(value: f64) -> Result<Self, Self::Error> {
        if value.is_finite() && value >= 0.0 {
            Ok(Self(value))
        } else {
            Err("rate must be a finite, non-negative number")
        }
    }
}

impl From<UsdRate> for f64 {
    fn from(rate: UsdRate) -> Self {
        rate.0
    }
}

/// The wire shape of the `costs` block. Field order and `BTreeMap` keys make its compact JSON
/// serialization canonical, which is what the version hash is computed over.
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct PriceTable {
    runners: RunnerPrices,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    daytona: Option<DaytonaRates>,
    #[serde(default)]
    vendors: BTreeMap<String, VendorPrice>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct RunnerPrices {
    default_usd_per_minute: UsdRate,
    #[serde(default)]
    labels: BTreeMap<String, UsdRate>,
}

/// Daytona sandbox unit prices.
#[derive(Clone, Copy, Debug, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DaytonaRates {
    /// USD per CPU-second of sandbox time.
    pub usd_per_cpu_second: UsdRate,
    /// USD per GB-second of sandbox memory.
    pub usd_per_ram_gb_second: UsdRate,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct VendorPrice {
    #[serde(rename = "match")]
    matcher: VendorMatch,
    usd_per_run: UsdRate,
}

/// How a vendor is recognised. Serde's externally tagged enum form accepts exactly one key, so
/// `{}` and `{ check_suite_app, review_login }` both fail to parse.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum VendorMatch {
    CheckSuiteApp(String),
    ReviewLogin(String),
}

/// A vendor that bills per run, as returned by the vendor lookups.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VendorRate<'table> {
    /// Configured vendor name.
    pub name: &'table str,
    /// USD charged for one run.
    pub usd_per_run: UsdRate,
}

/// The rate chosen for one workflow job.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RunnerRate {
    /// USD per billable minute.
    pub usd_per_minute: UsdRate,
    /// Whether no job label matched and the default rate was used.
    pub fell_back: bool,
}

/// Validated price table plus its version and the reverse vendor indexes.
pub struct CostsConfig {
    table: PriceTable,
    version: String,
    vendors_by_check_suite_app: HashMap<String, String>,
    vendors_by_review_login: HashMap<String, String>,
}

impl CostsConfig {
    /// Parses and validates a JSON price table.
    ///
    /// # Arguments
    ///
    /// * `json` - the `costs` block serialized as JSON.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] naming [`COSTS_VARIABLE`] when the JSON is malformed,
    /// has an unknown field, holds a negative or non-numeric rate, gives a vendor zero or two
    /// match rules or an empty one, or lets two vendors claim the same app slug or login. Parser
    /// messages are discarded because they can quote configured values.
    pub fn from_json(json: &str) -> Result<Self, ConfigError> {
        let invalid = || ConfigError::Invalid {
            variable: COSTS_VARIABLE,
        };
        let table: PriceTable = serde_json::from_str(json).map_err(|_| invalid())?;

        let mut vendors_by_check_suite_app = HashMap::new();
        let mut vendors_by_review_login = HashMap::new();
        for (name, vendor) in &table.vendors {
            let (index, key) = match &vendor.matcher {
                VendorMatch::CheckSuiteApp(slug) => (&mut vendors_by_check_suite_app, slug),
                VendorMatch::ReviewLogin(login) => (&mut vendors_by_review_login, login),
            };
            // `insert` returns the previous value, so `Some` means another vendor already
            // claimed this key and lookups would be ambiguous.
            if key.is_empty() || index.insert(key.clone(), name.clone()).is_some() {
                return Err(invalid());
            }
        }

        let canonical = serde_json::to_vec(&table).map_err(|_| invalid())?;
        let mut version = hex::encode(Sha256::digest(&canonical));
        version.truncate(PRICE_TABLE_VERSION_LENGTH);

        Ok(Self {
            table,
            version,
            vendors_by_check_suite_app,
            vendors_by_review_login,
        })
    }

    /// Returns `ci.price_table.version`: a short hash of the canonical table, stable across
    /// key order and whitespace and changed by any rate.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// Returns the rate for a job given its labels in job order.
    ///
    /// The first label present in `runners.labels` wins. With no match the default rate is
    /// returned and one `ci price missing` warning is logged with the bounded job labels.
    pub fn runner_rate(&self, labels: &[&str]) -> RunnerRate {
        let runners = &self.table.runners;
        if let Some(rate) = labels.iter().find_map(|label| runners.labels.get(*label)) {
            return RunnerRate {
                usd_per_minute: *rate,
                fell_back: false,
            };
        }

        let bounded_labels: Vec<&str> = labels
            .iter()
            .take(MAX_LOGGED_LABELS)
            .map(|label| truncate_chars(label, MAX_LOGGED_LABEL_CHARS))
            .collect();
        warn!(
            parent: None,
            labels = ?bounded_labels,
            labels_truncated = labels.len() > MAX_LOGGED_LABELS,
            default_usd_per_minute = runners.default_usd_per_minute.usd(),
            price_table_version = self.version.as_str(),
            "ci price missing"
        );
        RunnerRate {
            usd_per_minute: runners.default_usd_per_minute,
            fell_back: true,
        }
    }

    /// Returns the Daytona CPU-second and RAM-GB-second rates, when configured.
    pub fn daytona_rates(&self) -> Option<DaytonaRates> {
        self.table.daytona
    }

    /// Returns the vendor matched by a check-suite GitHub App slug.
    pub fn vendor_for_check_suite_app(&self, slug: &str) -> Option<VendorRate<'_>> {
        self.vendor_rate(self.vendors_by_check_suite_app.get(slug)?)
    }

    /// Returns the vendor matched by a pull-request reviewer login.
    pub fn vendor_for_review_login(&self, login: &str) -> Option<VendorRate<'_>> {
        self.vendor_rate(self.vendors_by_review_login.get(login)?)
    }

    fn vendor_rate(&self, name: &str) -> Option<VendorRate<'_>> {
        self.table
            .vendors
            .get_key_value(name)
            .map(|(name, vendor)| VendorRate {
                name,
                usd_per_run: vendor.usd_per_run,
            })
    }
}

impl fmt::Debug for CostsConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CostsConfig")
            .field("version", &self.version)
            .finish_non_exhaustive()
    }
}

/// Returns at most `max_chars` characters of `text`, cut on a character boundary.
fn truncate_chars(text: &str, max_chars: usize) -> &str {
    text.char_indices()
        .nth(max_chars)
        .map_or(text, |(byte_index, _)| &text[..byte_index])
}

#[cfg(test)]
mod tests {
    use std::{
        io::{self, Write},
        sync::{Arc, Mutex},
    };

    use tracing_subscriber::fmt::MakeWriter;

    use super::*;

    const EXAMPLE: &str = r#"{
        "runners": {
            "default_usd_per_minute": 0.012,
            "labels": { "ubuntu-latest-m": 0.012, "ubuntu-24.04-xl": 0.082 }
        },
        "daytona": { "usd_per_cpu_second": 0.000014, "usd_per_ram_gb_second": 0.0000045 },
        "vendors": {
            "example-review-bot": {
                "match": { "review_login": "review-bot[bot]" },
                "usd_per_run": 0.5
            },
            "example-visual-tests": {
                "match": { "check_suite_app": "visual-tests" },
                "usd_per_run": 0.25
            }
        }
    }"#;

    fn example() -> CostsConfig {
        CostsConfig::from_json(EXAMPLE).expect("example table is valid")
    }

    fn assert_invalid(json: &str) {
        assert_eq!(
            CostsConfig::from_json(json).err(),
            Some(ConfigError::Invalid {
                variable: COSTS_VARIABLE
            }),
            "{json}"
        );
    }

    #[derive(Clone, Default)]
    struct CapturedLogs(Arc<Mutex<Vec<u8>>>);

    impl CapturedLogs {
        fn text(&self) -> String {
            let bytes = self.0.lock().expect("captured logs lock is available");
            String::from_utf8(bytes.clone()).expect("captured logs are UTF-8")
        }
    }

    struct CapturedLogWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for CapturedLogWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| io::Error::other("captured logs lock was poisoned"))?
                .extend_from_slice(buffer);
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl<'writer> MakeWriter<'writer> for CapturedLogs {
        type Writer = CapturedLogWriter;

        fn make_writer(&'writer self) -> Self::Writer {
            CapturedLogWriter(Arc::clone(&self.0))
        }
    }

    fn with_captured_logs<T>(action: impl FnOnce() -> T) -> (T, String) {
        let captured_logs = CapturedLogs::default();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(captured_logs.clone())
            .finish();
        let result = tracing::subscriber::with_default(subscriber, action);
        (result, captured_logs.text())
    }

    #[test]
    fn runner_rate_uses_first_matching_label_in_job_order() {
        let costs = example();

        let (rate, logs) =
            with_captured_logs(|| costs.runner_rate(&["self-hosted", "ubuntu-24.04-xl"]));

        assert_eq!(
            rate,
            RunnerRate {
                usd_per_minute: UsdRate(0.082),
                fell_back: false
            }
        );
        assert_eq!(
            costs
                .runner_rate(&["ubuntu-latest-m", "ubuntu-24.04-xl"])
                .usd_per_minute,
            UsdRate(0.012)
        );
        assert!(!logs.contains("ci price missing"));
    }

    #[test]
    fn runner_rate_falls_back_and_logs_one_bounded_warning() {
        let costs = example();
        let long_label = "x".repeat(MAX_LOGGED_LABEL_CHARS + 10);
        let mut labels = vec![long_label.as_str()];
        labels.extend(["unpriced"; MAX_LOGGED_LABELS + 2]);

        let (rate, logs) = with_captured_logs(|| costs.runner_rate(&labels));

        assert_eq!(
            rate,
            RunnerRate {
                usd_per_minute: UsdRate(0.012),
                fell_back: true
            }
        );
        assert_eq!(logs.matches("ci price missing").count(), 1);
        assert!(logs.contains(&format!("\"{}\"", "x".repeat(MAX_LOGGED_LABEL_CHARS))));
        assert!(!logs.contains(&"x".repeat(MAX_LOGGED_LABEL_CHARS + 1)));
        assert_eq!(logs.matches("\"unpriced\"").count(), MAX_LOGGED_LABELS - 1);
        assert!(logs.contains("labels_truncated=true"));
        assert!(logs.contains(&format!("price_table_version=\"{}\"", costs.version())));
    }

    #[test]
    fn truncate_chars_cuts_on_character_boundaries() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("abc", 2), "ab");
        assert_eq!(truncate_chars("\u{e9}\u{e9}\u{e9}", 2), "\u{e9}\u{e9}");
    }

    #[test]
    fn daytona_and_vendor_lookups_return_configured_values() {
        let costs = example();

        assert_eq!(
            costs.daytona_rates(),
            Some(DaytonaRates {
                usd_per_cpu_second: UsdRate(0.000014),
                usd_per_ram_gb_second: UsdRate(0.0000045),
            })
        );
        assert_eq!(
            costs.vendor_for_check_suite_app("visual-tests"),
            Some(VendorRate {
                name: "example-visual-tests",
                usd_per_run: UsdRate(0.25)
            })
        );
        assert_eq!(
            costs.vendor_for_review_login("review-bot[bot]"),
            Some(VendorRate {
                name: "example-review-bot",
                usd_per_run: UsdRate(0.5)
            })
        );
        assert_eq!(costs.vendor_for_check_suite_app("review-bot[bot]"), None);
        assert_eq!(costs.vendor_for_review_login("visual-tests"), None);
    }

    #[test]
    fn minimal_table_needs_only_a_default_runner_rate() {
        let costs = CostsConfig::from_json(r#"{"runners":{"default_usd_per_minute":0}}"#)
            .expect("minimal table is valid");

        assert_eq!(costs.daytona_rates(), None);
        assert_eq!(costs.vendor_for_check_suite_app("anything"), None);
        assert_eq!(costs.runner_rate(&[]).usd_per_minute, UsdRate(0.0));
    }

    #[test]
    fn invalid_tables_fail_without_leaking_values() {
        let vendor = |matcher: &str| {
            format!(
                r#"{{"runners":{{"default_usd_per_minute":1}},
                    "vendors":{{"v":{{"match":{matcher},"usd_per_run":1}}}}}}"#
            )
        };
        for json in [
            "",
            "not json",
            "[]",
            "{}",
            r#"{"runners":{"default_usd_per_minute":-0.01}}"#,
            r#"{"runners":{"default_usd_per_minute":"0.01"}}"#,
            r#"{"runners":{"default_usd_per_minute":1,"labels":{"a":-1}}}"#,
            r#"{"runners":{"default_usd_per_minute":1e400}}"#,
            r#"{"runners":{"default_usd_per_minute":1},"extra":1}"#,
            r#"{"runners":{"default_usd_per_minute":1,"label":{}}}"#,
            r#"{"runners":{"default_usd_per_minute":1},
                "daytona":{"usd_per_cpu_second":1,"usd_per_ram_gb_second":-1}}"#,
            &vendor("{}"),
            &vendor(r#"{"check_suite_app":"a","review_login":"b"}"#),
            &vendor(r#"{"check_suite_app":""}"#),
            &vendor(r#"{"other":"a"}"#),
        ] {
            assert_invalid(json);
        }
        let error = CostsConfig::from_json(r#"{"runners":{"default_usd_per_minute":"s3cr3t"}}"#)
            .expect_err("string rate is invalid");
        assert!(!error.to_string().contains("s3cr3t"));
        assert!(error.to_string().contains(COSTS_VARIABLE));
    }

    #[test]
    fn two_vendors_cannot_claim_the_same_key() {
        assert_invalid(
            r#"{"runners":{"default_usd_per_minute":1},"vendors":{
                "a":{"match":{"review_login":"bot"},"usd_per_run":1},
                "b":{"match":{"review_login":"bot"},"usd_per_run":2}}}"#,
        );
        CostsConfig::from_json(
            r#"{"runners":{"default_usd_per_minute":1},"vendors":{
                "a":{"match":{"review_login":"bot"},"usd_per_run":1},
                "b":{"match":{"check_suite_app":"bot"},"usd_per_run":2}}}"#,
        )
        .expect("the same text may name one app and one login");
    }

    #[test]
    fn version_ignores_formatting_and_tracks_rates() {
        let reordered = r#"{"vendors":{"example-visual-tests":{"usd_per_run":0.25,
            "match":{"check_suite_app":"visual-tests"}},"example-review-bot":{"usd_per_run":5e-1,
            "match":{"review_login":"review-bot[bot]"}}},"daytona":{"usd_per_ram_gb_second":
            0.0000045,"usd_per_cpu_second":0.000014},"runners":{"labels":{"ubuntu-24.04-xl":0.082,
            "ubuntu-latest-m":0.0120},"default_usd_per_minute":0.012}}"#;
        let changed = EXAMPLE.replace("0.082", "0.083");

        let version = example().version().to_owned();

        assert_eq!(version.len(), PRICE_TABLE_VERSION_LENGTH);
        assert!(version.bytes().all(|byte| byte.is_ascii_hexdigit()));
        assert_eq!(
            CostsConfig::from_json(reordered)
                .expect("reordered table is valid")
                .version(),
            version
        );
        assert_ne!(
            CostsConfig::from_json(&changed)
                .expect("changed table is valid")
                .version(),
            version
        );
    }
}
