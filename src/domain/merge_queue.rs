use std::fmt;

use thiserror::Error;
use time::{
    format_description::{well_known::Rfc3339, FormatItem},
    macros::format_description,
    OffsetDateTime, UtcOffset,
};

const QUEUE_TIMESTAMP_FORMAT: &[FormatItem<'static>] =
    format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:3]Z");

/// A positive GitHub pull-request number.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PullRequestNumber(i64);

impl PullRequestNumber {
    /// Constructs a pull-request number from a positive integer.
    ///
    /// # Errors
    ///
    /// Returns [`PullRequestNumberError`] when `value` is zero or negative.
    pub fn new(value: i64) -> Result<Self, PullRequestNumberError> {
        if value > 0 {
            Ok(Self(value))
        } else {
            Err(PullRequestNumberError)
        }
    }

    /// Returns the positive integer used by GitHub and SQLite.
    pub fn get(self) -> i64 {
        self.0
    }
}

/// A non-positive pull-request number.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("pull-request number must be positive")]
pub struct PullRequestNumberError;

/// A validated RFC 3339 event timestamp normalized to UTC milliseconds.
#[derive(Clone, PartialEq, Eq)]
pub struct QueueTimestamp(String);

impl QueueTimestamp {
    /// Parses and normalizes an RFC 3339 timestamp.
    ///
    /// # Errors
    ///
    /// Returns [`QueueTimestampError`] when `value` is malformed or cannot be represented in the
    /// canonical UTC millisecond format used for SQLite ordering.
    pub fn parse(value: &str) -> Result<Self, QueueTimestampError> {
        let timestamp = OffsetDateTime::parse(value, &Rfc3339).map_err(|_| QueueTimestampError)?;
        Self::from_datetime(timestamp)
    }

    /// Normalizes a timestamp to the canonical UTC millisecond persistence format.
    ///
    /// # Errors
    ///
    /// Returns [`QueueTimestampError`] when the timestamp's year cannot be represented by the
    /// canonical format.
    pub fn from_datetime(timestamp: OffsetDateTime) -> Result<Self, QueueTimestampError> {
        timestamp
            .to_offset(UtcOffset::UTC)
            .format(QUEUE_TIMESTAMP_FORMAT)
            .map(Self)
            .map_err(|_| QueueTimestampError)
    }

    /// Returns the canonical UTC timestamp text.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for QueueTimestamp {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("QueueTimestamp([REDACTED])")
    }
}

/// An invalid merge-queue event timestamp.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("merge-queue timestamp is invalid")]
pub struct QueueTimestampError;

/// A bounded durable merge-queue attempt outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueOutcome {
    /// The attempt is active and has no completion timestamp.
    Pending,
    /// GitHub reported that the pull request merged.
    Succeeded,
    /// Reserved for a future evidence-backed failure classifier.
    Failed,
    /// Reserved for a future evidence-backed cancellation classifier.
    Cancelled,
    /// The attempt ended without a supported semantic classification.
    Unknown,
}

impl QueueOutcome {
    /// Parses the fixed persistence vocabulary.
    ///
    /// # Errors
    ///
    /// Returns [`QueueValueError`] for every value outside the bounded outcome set.
    pub fn parse(value: &str) -> Result<Self, QueueValueError> {
        match value {
            "pending" => Ok(Self::Pending),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            "unknown" => Ok(Self::Unknown),
            _ => Err(QueueValueError),
        }
    }

    /// Returns the fixed SQLite and metrics representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
            Self::Unknown => "unknown",
        }
    }
}

/// A bounded durable merge-queue transition reason.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueReasonCode {
    /// No terminal reason exists for a pending attempt.
    None,
    /// A merged pull-request event proved success.
    PullRequestMerged,
    /// A dequeue ended the attempt without evidence for a stronger classification.
    UnclassifiedDequeue,
}

impl QueueReasonCode {
    /// Parses the fixed persistence vocabulary.
    ///
    /// # Errors
    ///
    /// Returns [`QueueValueError`] for every value outside the bounded reason set.
    pub fn parse(value: &str) -> Result<Self, QueueValueError> {
        match value {
            "none" => Ok(Self::None),
            "pull_request_merged" => Ok(Self::PullRequestMerged),
            "unclassified_dequeue" => Ok(Self::UnclassifiedDequeue),
            _ => Err(QueueValueError),
        }
    }

    /// Returns the fixed SQLite and metrics representation.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::PullRequestMerged => "pull_request_merged",
            Self::UnclassifiedDequeue => "unclassified_dequeue",
        }
    }
}

/// A value outside a bounded merge-queue outcome or reason vocabulary.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("merge-queue value is not supported")]
pub struct QueueValueError;

/// An evidence-backed terminal transition accepted by the Phase 3 persistence boundary.
///
/// Fields are private so callers cannot combine an outcome, reason, and completion timestamp into
/// an unsupported state. Phase 3 deliberately exposes no `failed` or `cancelled` constructor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueCompletion {
    completed_at: QueueTimestamp,
    outcome: QueueOutcome,
    reason_code: QueueReasonCode,
}

impl QueueCompletion {
    /// Creates a successful completion proven by a merged pull-request event.
    pub fn pull_request_merged(completed_at: QueueTimestamp) -> Self {
        Self {
            completed_at,
            outcome: QueueOutcome::Succeeded,
            reason_code: QueueReasonCode::PullRequestMerged,
        }
    }

    /// Creates the deliberately unknown completion used for every Phase 3 dequeue.
    pub fn unclassified_dequeue(completed_at: QueueTimestamp) -> Self {
        Self {
            completed_at,
            outcome: QueueOutcome::Unknown,
            reason_code: QueueReasonCode::UnclassifiedDequeue,
        }
    }

    /// Returns the validated completion timestamp.
    pub fn completed_at(&self) -> &QueueTimestamp {
        &self.completed_at
    }

    /// Returns the bounded terminal outcome.
    pub fn outcome(&self) -> QueueOutcome {
        self.outcome
    }

    /// Returns the bounded terminal reason code.
    pub fn reason_code(&self) -> QueueReasonCode {
        self.reason_code
    }
}
