//! Bounded workflow telemetry values used to project authenticated GitHub Actions history.
#![allow(dead_code)]

use std::{fmt, time::SystemTime};

use opentelemetry::trace::Status;
use thiserror::Error;

use crate::{
    domain::{delivery::DeliveryId, merge_queue::PullRequestNumber},
    security::CanonicalRepositoryName,
};

use super::trace::CommitSha;

const MAX_DISPLAY_NAME_LENGTH: usize = 128;

/// A malformed workflow telemetry value.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("workflow telemetry value must be positive")]
pub(crate) struct WorkflowValueError;

macro_rules! positive_i64_newtype {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Copy, PartialEq, Eq)]
        pub(crate) struct $name(i64);

        impl $name {
            /// Creates a validated positive workflow identifier.
            ///
            /// # Parameters
            ///
            /// * `value` - The candidate identifier.
            ///
            /// # Returns
            ///
            /// A bounded identifier when `value` is greater than zero.
            ///
            /// # Errors
            ///
            /// Returns [`WorkflowValueError`] when `value` is zero or negative.
            pub(crate) fn new(value: i64) -> Result<Self, WorkflowValueError> {
                if value > 0 {
                    Ok(Self(value))
                } else {
                    Err(WorkflowValueError)
                }
            }

            /// Returns the validated positive integer.
            pub(crate) const fn get(self) -> i64 {
                self.0
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(concat!(stringify!($name), "([REDACTED])"))
            }
        }
    };
}

positive_i64_newtype!(
    WorkflowRunId,
    "A validated GitHub Actions workflow run identifier."
);
positive_i64_newtype!(
    WorkflowRunAttempt,
    "A validated GitHub Actions workflow run attempt."
);
positive_i64_newtype!(
    WorkflowJobId,
    "A validated GitHub Actions workflow job identifier."
);

/// A sanitized GitHub Actions display name.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DisplayName(String);

impl DisplayName {
    /// Removes Unicode control characters and retains at most the first 128 visible characters.
    ///
    /// # Parameters
    ///
    /// * `value` - The candidate workflow or step display name.
    ///
    /// # Returns
    ///
    /// A sanitized display name when at least one visible character remains, otherwise `None`.
    pub(crate) fn sanitize(value: &str) -> Option<Self> {
        let mut sanitized = String::with_capacity(value.len().min(MAX_DISPLAY_NAME_LENGTH));
        let mut retained = 0usize;

        for character in value.chars() {
            if character.is_control() {
                continue;
            }
            if retained == MAX_DISPLAY_NAME_LENGTH {
                break;
            }
            sanitized.push(character);
            retained += 1;
        }

        if sanitized.is_empty() {
            None
        } else {
            Some(Self(sanitized))
        }
    }

    /// Returns the sanitized display name.
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for DisplayName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DisplayName([REDACTED])")
    }
}

/// A bounded GitHub Actions conclusion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WorkflowConclusion {
    /// The job or step succeeded.
    Success,
    /// The job or step failed.
    Failure,
    /// The job or step was cancelled.
    Cancelled,
    /// The job or step was skipped.
    Skipped,
    /// The job or step timed out.
    TimedOut,
    /// The job or step concluded neutrally.
    Neutral,
    /// Any conclusion outside the approved vocabulary.
    Other,
}

impl WorkflowConclusion {
    /// Normalizes a raw GitHub conclusion into the bounded workflow vocabulary.
    ///
    /// # Parameters
    ///
    /// * `value` - The raw conclusion text, or `None` when the input is missing.
    ///
    /// # Returns
    ///
    /// The bounded workflow conclusion.
    pub(crate) fn normalize(value: Option<&str>) -> Self {
        match value {
            Some("success") => Self::Success,
            Some("failure") => Self::Failure,
            Some("cancelled") => Self::Cancelled,
            Some("skipped") => Self::Skipped,
            Some("timed_out") => Self::TimedOut,
            Some("neutral") => Self::Neutral,
            _ => Self::Other,
        }
    }

    /// Returns the GitHub conclusion vocabulary value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
            Self::TimedOut => "timed_out",
            Self::Neutral => "neutral",
            Self::Other => "other",
        }
    }

    /// Returns the semantic-convention result when one exists.
    ///
    /// The result is omitted for neutral and unsupported conclusions.
    pub(crate) const fn semantic_result(self) -> Option<&'static str> {
        match self {
            Self::Success => Some("success"),
            Self::Failure => Some("failure"),
            Self::Cancelled => Some("cancellation"),
            Self::Skipped => Some("skip"),
            Self::TimedOut => Some("timeout"),
            Self::Neutral | Self::Other => None,
        }
    }

    /// Returns the bounded OpenTelemetry status for this conclusion.
    ///
    /// `failure` and `timed_out` are recorded as an error; `success` is recorded as ok; the
    /// remaining conclusions leave status unset.
    pub(crate) fn status(self) -> Status {
        match self {
            Self::Success => Status::Ok,
            Self::Failure | Self::TimedOut => Status::error("workflow_failed"),
            Self::Cancelled | Self::Skipped | Self::Neutral | Self::Other => Status::Unset,
        }
    }
}

/// The source used to select a historical timing interval.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TimingSource {
    /// The timestamps were accepted from the GitHub payload.
    Reported,
    /// The timestamps were synthesized from a bounded fallback.
    Fallback,
}

impl TimingSource {
    /// Returns the normalized timing-source value.
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Reported => "reported",
            Self::Fallback => "fallback",
        }
    }
}

/// A bounded historical interval and the source that selected it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoricalTiming {
    pub(crate) start: SystemTime,
    pub(crate) end: SystemTime,
    pub(crate) source: TimingSource,
}

/// An owned workflow-job trace accepted by the telemetry emitter.
#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkflowJobTrace {
    pub(crate) repository_name: CanonicalRepositoryName,
    pub(crate) delivery_id: DeliveryId,
    pub(crate) workflow_name: Option<DisplayName>,
    pub(crate) run_id: WorkflowRunId,
    pub(crate) run_attempt: WorkflowRunAttempt,
    pub(crate) job_id: WorkflowJobId,
    pub(crate) job_name: Option<DisplayName>,
    pub(crate) conclusion: WorkflowConclusion,
    pub(crate) head_sha: Option<CommitSha>,
    pub(crate) pull_requests: Vec<PullRequestNumber>,
    pub(crate) timing: HistoricalTiming,
    pub(crate) steps: Vec<WorkflowStepTrace>,
}

/// An owned workflow-step trace accepted by the telemetry emitter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WorkflowStepTrace {
    pub(crate) number: i64,
    pub(crate) name: Option<DisplayName>,
    pub(crate) conclusion: WorkflowConclusion,
    pub(crate) timing: HistoricalTiming,
}

impl WorkflowStepTrace {
    /// Creates a bounded workflow step trace.
    ///
    /// # Parameters
    ///
    /// * `number` - The positive step number.
    /// * `name` - The optional sanitized step name.
    /// * `conclusion` - The bounded step conclusion.
    /// * `timing` - The selected historical timing interval.
    ///
    /// # Returns
    ///
    /// A bounded step trace when `number` is positive.
    ///
    /// # Errors
    ///
    /// Returns [`WorkflowValueError`] when `number` is zero or negative.
    pub(crate) fn new(
        number: i64,
        name: Option<DisplayName>,
        conclusion: WorkflowConclusion,
        timing: HistoricalTiming,
    ) -> Result<Self, WorkflowValueError> {
        if number > 0 {
            Ok(Self {
                number,
                name,
                conclusion,
                timing,
            })
        } else {
            Err(WorkflowValueError)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use opentelemetry::trace::Status;

    use super::{
        DisplayName, HistoricalTiming, TimingSource, WorkflowConclusion, WorkflowJobId,
        WorkflowRunAttempt, WorkflowRunId, WorkflowStepTrace,
    };

    #[test]
    fn positive_workflow_identifiers_reject_zero_and_negative_values() {
        assert!(WorkflowRunId::new(1).is_ok());
        assert!(WorkflowRunAttempt::new(1).is_ok());
        assert!(WorkflowJobId::new(1).is_ok());
        assert!(WorkflowRunId::new(0).is_err());
        assert!(WorkflowRunAttempt::new(-1).is_err());
        assert!(WorkflowJobId::new(0).is_err());
    }

    #[test]
    fn display_names_remove_controls_and_stop_after_128_characters() {
        let input = format!("alpha\n{}omega", "x".repeat(200));
        let name = DisplayName::sanitize(&input).expect("visible characters remain");
        assert_eq!(name.as_str().chars().count(), 128);
        assert!(!name.as_str().chars().any(char::is_control));
        assert_eq!(DisplayName::sanitize("\n\r\t"), None);
    }

    #[test]
    fn conclusions_have_a_closed_normalized_vocabulary() {
        let cases = [
            (
                Some("success"),
                WorkflowConclusion::Success,
                Some("success"),
            ),
            (
                Some("failure"),
                WorkflowConclusion::Failure,
                Some("failure"),
            ),
            (
                Some("cancelled"),
                WorkflowConclusion::Cancelled,
                Some("cancellation"),
            ),
            (Some("skipped"), WorkflowConclusion::Skipped, Some("skip")),
            (
                Some("timed_out"),
                WorkflowConclusion::TimedOut,
                Some("timeout"),
            ),
            (Some("neutral"), WorkflowConclusion::Neutral, None),
            (Some("private-unknown"), WorkflowConclusion::Other, None),
            (None, WorkflowConclusion::Other, None),
        ];
        for (raw, expected, semantic_result) in cases {
            let conclusion = WorkflowConclusion::normalize(raw);
            assert_eq!(conclusion, expected);
            assert_eq!(conclusion.semantic_result(), semantic_result);
        }
    }

    #[test]
    fn workflow_conclusions_map_to_status_and_strings() {
        let cases = [
            (
                WorkflowConclusion::Success,
                "success",
                Some("success"),
                Some(Status::Ok),
            ),
            (
                WorkflowConclusion::Failure,
                "failure",
                Some("failure"),
                Some(Status::error("workflow_failed")),
            ),
            (
                WorkflowConclusion::Cancelled,
                "cancelled",
                Some("cancellation"),
                Some(Status::Unset),
            ),
            (
                WorkflowConclusion::Skipped,
                "skipped",
                Some("skip"),
                Some(Status::Unset),
            ),
            (
                WorkflowConclusion::TimedOut,
                "timed_out",
                Some("timeout"),
                Some(Status::error("workflow_failed")),
            ),
            (
                WorkflowConclusion::Neutral,
                "neutral",
                None,
                Some(Status::Unset),
            ),
            (
                WorkflowConclusion::Other,
                "other",
                None,
                Some(Status::Unset),
            ),
        ];

        for (conclusion, expected, semantic_result, expected_status) in cases {
            assert_eq!(conclusion.as_str(), expected);
            assert_eq!(conclusion.semantic_result(), semantic_result);
            assert_eq!(
                conclusion.status(),
                expected_status.expect("status is always available")
            );
        }
    }

    #[test]
    fn workflow_value_debug_output_is_redacted() {
        let display_name = DisplayName::sanitize("Build Workflow").expect("display name is valid");
        let run_id = WorkflowRunId::new(7).expect("run id is valid");
        let run_attempt = WorkflowRunAttempt::new(2).expect("run attempt is valid");
        let job_id = WorkflowJobId::new(9).expect("job id is valid");

        assert!(!format!("{display_name:?}").contains("Build Workflow"));
        assert!(!format!("{run_id:?}").contains('7'));
        assert!(!format!("{run_attempt:?}").contains('2'));
        assert!(!format!("{job_id:?}").contains('9'));
    }

    #[test]
    fn workflow_step_trace_rejects_non_positive_numbers() {
        let timing = HistoricalTiming {
            start: SystemTime::UNIX_EPOCH,
            end: SystemTime::UNIX_EPOCH + Duration::from_secs(1),
            source: TimingSource::Reported,
        };

        assert!(
            WorkflowStepTrace::new(1, None, WorkflowConclusion::Success, timing.clone()).is_ok()
        );
        assert!(
            WorkflowStepTrace::new(0, None, WorkflowConclusion::Success, timing.clone()).is_err()
        );
        assert!(WorkflowStepTrace::new(-1, None, WorkflowConclusion::Success, timing).is_err());
    }
}
