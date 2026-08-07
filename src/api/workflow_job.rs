use std::time::{Duration, SystemTime};

use serde::Deserialize;
use serde_json::Value;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::{
    domain::{delivery::DeliveryId, merge_queue::PullRequestNumber},
    security::CanonicalRepositoryName,
    telemetry::{
        trace::CommitSha,
        workflow::{
            DisplayName, HistoricalTiming, WorkflowConclusion, WorkflowJobId, WorkflowJobTrace,
            WorkflowJobTraceParts, WorkflowPullRequests, WorkflowRunAttempt, WorkflowRunId,
            WorkflowStepTrace,
        },
    },
};

#[derive(Deserialize)]
struct WorkflowJobEnvelope {
    workflow_job: WorkflowJobProjection,
}

#[derive(Deserialize)]
struct WorkflowJobProjection {
    id: i64,
    run_id: i64,
    run_attempt: i64,
    workflow_name: Option<Value>,
    name: Option<Value>,
    conclusion: Option<Value>,
    head_sha: Option<Value>,
    started_at: Option<Value>,
    completed_at: Option<Value>,
    #[serde(default)]
    pull_requests: Vec<PullRequestProjection>,
    #[serde(default)]
    steps: Vec<WorkflowStepProjection>,
}

#[derive(Deserialize)]
struct PullRequestProjection {
    number: i64,
}

#[derive(Deserialize)]
struct WorkflowStepProjection {
    number: i64,
    name: Option<Value>,
    conclusion: Option<Value>,
    started_at: Option<Value>,
    completed_at: Option<Value>,
}

/// Projects one authenticated completed workflow-job payload into the bounded workflow model.
///
/// # Parameters
///
/// * `body` - The authenticated JSON request body.
/// * `repository_name` - The canonical authenticated repository name.
/// * `delivery_id` - The authenticated GitHub delivery identifier.
/// * `received_at` - The bounded webhook receipt timestamp.
///
/// # Returns
///
/// A bounded workflow job trace when the wrapper and required workflow-job identifiers are valid;
/// otherwise `None`.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn project_completed_job(
    body: &[u8],
    repository_name: &CanonicalRepositoryName,
    delivery_id: &DeliveryId,
    received_at: OffsetDateTime,
) -> Option<WorkflowJobTrace> {
    let received_at = offset_datetime_to_system_time(received_at)?;
    let envelope: WorkflowJobEnvelope = serde_json::from_slice(body).ok()?;
    let WorkflowJobProjection {
        id,
        run_id,
        run_attempt,
        workflow_name,
        name,
        conclusion,
        head_sha,
        started_at,
        completed_at,
        pull_requests,
        steps,
    } = envelope.workflow_job;

    let run_id = WorkflowRunId::new(run_id).ok()?;
    let run_attempt = WorkflowRunAttempt::new(run_attempt).ok()?;
    let job_id = WorkflowJobId::new(id).ok()?;
    let timing = select_job_timing(
        parse_timestamp(started_at.as_ref()),
        parse_timestamp(completed_at.as_ref()),
        received_at,
    );
    let mut projected_steps = Vec::with_capacity(steps.len());
    for step in steps {
        let timing = select_step_timing(
            parse_timestamp(step.started_at.as_ref()),
            parse_timestamp(step.completed_at.as_ref()),
            &timing,
        );
        if let Ok(step) = WorkflowStepTrace::new(
            step.number,
            sanitize_display_name(step.name.as_ref()),
            normalize_conclusion(step.conclusion.as_ref()),
            timing,
        ) {
            projected_steps.push(step);
        }
    }

    Some(WorkflowJobTrace::new(WorkflowJobTraceParts {
        repository_name: repository_name.clone(),
        delivery_id: *delivery_id,
        workflow_name: sanitize_display_name(workflow_name.as_ref()),
        run_id,
        run_attempt,
        job_id,
        job_name: sanitize_display_name(name.as_ref()),
        conclusion: normalize_conclusion(conclusion.as_ref()),
        head_sha: parse_commit_sha(head_sha.as_ref()),
        pull_requests: WorkflowPullRequests::new(positive_pull_requests(&pull_requests)),
        timing,
        steps: projected_steps,
    }))
}

fn parse_timestamp(value: Option<&Value>) -> Option<SystemTime> {
    value
        .and_then(Value::as_str)
        .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
        .and_then(offset_datetime_to_system_time)
}

fn select_job_timing(
    start: Option<SystemTime>,
    end: Option<SystemTime>,
    received_at: SystemTime,
) -> HistoricalTiming {
    if let (Some(start), Some(end)) = (start, end) {
        HistoricalTiming::reported(start, end).unwrap_or_else(|_| HistoricalTiming::fallback(end))
    } else if let Some(end) = end {
        HistoricalTiming::fallback(end)
    } else {
        HistoricalTiming::fallback(received_at)
    }
}

fn select_step_timing(
    start: Option<SystemTime>,
    end: Option<SystemTime>,
    parent: &HistoricalTiming,
) -> HistoricalTiming {
    if let (Some(start), Some(end)) = (start, end) {
        HistoricalTiming::reported_within_parent(start, end, parent)
            .unwrap_or_else(|_| HistoricalTiming::fallback(parent.end()))
    } else {
        HistoricalTiming::fallback(parent.end())
    }
}

fn positive_pull_requests(values: &[PullRequestProjection]) -> Vec<PullRequestNumber> {
    let mut pull_requests = Vec::with_capacity(values.len().min(20));
    for value in values {
        let Ok(number) = PullRequestNumber::new(value.number) else {
            continue;
        };
        if pull_requests.len() == 20 {
            break;
        }
        pull_requests.push(number);
    }
    pull_requests
}

fn sanitize_display_name(value: Option<&Value>) -> Option<DisplayName> {
    value
        .and_then(Value::as_str)
        .and_then(DisplayName::sanitize)
}

fn normalize_conclusion(value: Option<&Value>) -> WorkflowConclusion {
    WorkflowConclusion::normalize(value.and_then(Value::as_str))
}

fn parse_commit_sha(value: Option<&Value>) -> Option<CommitSha> {
    value
        .and_then(Value::as_str)
        .and_then(|value| CommitSha::parse(value).ok())
}

fn offset_datetime_to_system_time(value: OffsetDateTime) -> Option<SystemTime> {
    let seconds = value.unix_timestamp();
    let nanoseconds = value.nanosecond();

    if seconds >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(Duration::new(seconds as u64, nanoseconds))
    } else if nanoseconds == 0 {
        SystemTime::UNIX_EPOCH.checked_sub(Duration::from_secs(seconds.unsigned_abs()))
    } else {
        let whole_seconds_before_epoch = seconds.unsigned_abs() - 1;
        let nanoseconds_before_epoch = 1_000_000_000 - nanoseconds;
        SystemTime::UNIX_EPOCH.checked_sub(Duration::new(
            whole_seconds_before_epoch,
            nanoseconds_before_epoch,
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use serde_json::{json, Value};
    use time::{macros::datetime, OffsetDateTime};

    use super::{offset_datetime_to_system_time, project_completed_job};
    use crate::{
        domain::delivery::DeliveryId,
        security::CanonicalRepositoryName,
        telemetry::workflow::{TimingSource, WorkflowConclusion},
    };

    fn repository_name() -> CanonicalRepositoryName {
        CanonicalRepositoryName::new("Owner/Repository").expect("repository name is valid")
    }

    fn delivery_id() -> DeliveryId {
        DeliveryId::parse("550e8400-e29b-41d4-a716-446655440000").expect("delivery id is valid")
    }

    fn project_fixture_at(
        body: Value,
        received_at: OffsetDateTime,
    ) -> Option<crate::telemetry::workflow::WorkflowJobTrace> {
        let bytes = serde_json::to_vec(&body).expect("fixture JSON serializes");
        project_completed_job(&bytes, &repository_name(), &delivery_id(), received_at)
    }

    fn project_fixture(body: Value) -> Option<crate::telemetry::workflow::WorkflowJobTrace> {
        project_fixture_at(body, datetime!(2026-08-06 10:06:00 UTC))
    }

    #[test]
    fn completed_projection_keeps_only_validated_bounded_fields() {
        let trace = project_fixture(json!({
            "workflow_job": {
                "id": 41,
                "run_id": 31,
                "run_attempt": 2,
                "workflow_name": "Build\nWorkflow",
                "name": "Linux\tJob",
                "conclusion": "success",
                "head_sha": "0123456789abcdef0123456789abcdef01234567",
                "started_at": "2026-08-06T10:00:00Z",
                "completed_at": "2026-08-06T10:05:00Z",
                "pull_requests": [{"number": 7}, {"number": -1}],
                "steps": [{
                    "number": 1,
                    "name": "Checkout\n",
                    "conclusion": "success",
                    "started_at": "2026-08-06T10:00:00Z",
                    "completed_at": "2026-08-06T10:01:00Z"
                }]
            }
        }))
        .expect("valid completed job projects");

        assert_eq!(trace.run_id().get(), 31);
        assert_eq!(trace.run_attempt().get(), 2);
        assert_eq!(trace.job_id().get(), 41);
        assert_eq!(trace.pull_requests().len(), 1);
        assert_eq!(trace.steps().len(), 1);
        assert_eq!(trace.timing().source(), TimingSource::Reported);
        assert_eq!(trace.steps()[0].timing().source(), TimingSource::Reported);
        assert_eq!(trace.conclusion(), WorkflowConclusion::Success);
        assert_eq!(
            trace.workflow_name().map(|value| value.as_str()),
            Some("BuildWorkflow")
        );
        assert_eq!(
            trace.job_name().map(|value| value.as_str()),
            Some("LinuxJob")
        );
        assert_eq!(
            trace.steps()[0].name().map(|value| value.as_str()),
            Some("Checkout")
        );
    }

    #[test]
    fn zero_or_negative_required_ids_reject_projection() {
        for (run_id, run_attempt, job_id) in [(0, 2, 41), (31, 0, 41), (31, 2, -1)] {
            assert!(
                project_fixture(json!({
                    "workflow_job": {
                        "id": job_id,
                        "run_id": run_id,
                        "run_attempt": run_attempt,
                        "completed_at": "2026-08-06T10:05:00Z",
                        "steps": []
                    }
                }))
                .is_none(),
                "accepted invalid ids run_id={run_id} run_attempt={run_attempt} job_id={job_id}"
            );
        }
    }

    #[test]
    fn malformed_or_missing_job_timestamps_select_fallback() {
        let trace = project_fixture(json!({
            "workflow_job": {
                "id": 41,
                "run_id": 31,
                "run_attempt": 2,
                "started_at": 42,
                "steps": []
            }
        }))
        .expect("projection falls back to receipt time");

        let expected = offset_datetime_to_system_time(datetime!(2026-08-06 10:06:00 UTC))
            .expect("expected system time is representable");
        assert_eq!(trace.timing().source(), TimingSource::Fallback);
        assert_eq!(trace.timing().start(), expected);
        assert_eq!(trace.timing().end(), expected);
    }

    #[test]
    fn reversed_job_timestamps_fall_back_at_valid_completion() {
        let trace = project_fixture(json!({
            "workflow_job": {
                "id": 41,
                "run_id": 31,
                "run_attempt": 2,
                "started_at": "2026-08-06T10:05:00Z",
                "completed_at": "2026-08-06T10:04:00Z",
                "steps": []
            }
        }))
        .expect("projection falls back at completion time");

        let expected = offset_datetime_to_system_time(datetime!(2026-08-06 10:04:00 UTC))
            .expect("expected system time is representable");
        assert_eq!(trace.timing().source(), TimingSource::Fallback);
        assert_eq!(trace.timing().start(), expected);
        assert_eq!(trace.timing().end(), expected);
    }

    #[test]
    fn reversed_and_out_of_parent_step_timestamps_fall_back_at_selected_job_end() {
        let trace = project_fixture(json!({
            "workflow_job": {
                "id": 41,
                "run_id": 31,
                "run_attempt": 2,
                "started_at": "2026-08-06T10:00:00Z",
                "completed_at": "2026-08-06T10:05:00Z",
                "steps": [
                    {
                        "number": 1,
                        "started_at": "2026-08-06T10:02:00Z",
                        "completed_at": "2026-08-06T10:01:00Z"
                    },
                    {
                        "number": 2,
                        "started_at": "2026-08-06T09:59:00Z",
                        "completed_at": "2026-08-06T10:01:00Z"
                    }
                ]
            }
        }))
        .expect("projection retains steps with bounded fallback timing");

        let expected = offset_datetime_to_system_time(datetime!(2026-08-06 10:05:00 UTC))
            .expect("expected system time is representable");
        assert_eq!(trace.steps().len(), 2);
        for step in trace.steps() {
            assert_eq!(step.timing().source(), TimingSource::Fallback);
            assert_eq!(step.timing().start(), expected);
            assert_eq!(step.timing().end(), expected);
        }
    }

    #[test]
    fn only_the_first_twenty_positive_pull_requests_are_retained() {
        let pull_requests = (1..=25)
            .map(|number| json!({ "number": number }))
            .collect::<Vec<_>>();
        let trace = project_fixture(json!({
            "workflow_job": {
                "id": 41,
                "run_id": 31,
                "run_attempt": 2,
                "completed_at": "2026-08-06T10:05:00Z",
                "pull_requests": pull_requests,
                "steps": []
            }
        }))
        .expect("projection keeps bounded pull requests");

        assert_eq!(trace.pull_requests().len(), 20);
        assert_eq!(
            trace
                .pull_requests()
                .iter()
                .map(|number| number.get())
                .collect::<Vec<_>>(),
            (1..=20).collect::<Vec<_>>()
        );
    }

    #[test]
    fn every_valid_step_is_retained_in_input_order() {
        let trace = project_fixture(json!({
            "workflow_job": {
                "id": 41,
                "run_id": 31,
                "run_attempt": 2,
                "completed_at": "2026-08-06T10:05:00Z",
                "steps": [
                    {"number": 2, "name": "second", "completed_at": "2026-08-06T10:05:00Z"},
                    {"number": 1, "name": "first", "completed_at": "2026-08-06T10:05:00Z"},
                    {"number": 3, "name": "third", "completed_at": "2026-08-06T10:05:00Z"}
                ]
            }
        }))
        .expect("projection keeps all valid steps");

        assert_eq!(
            trace
                .steps()
                .iter()
                .map(|step| step.number())
                .collect::<Vec<_>>(),
            vec![2, 1, 3]
        );
        assert_eq!(
            trace
                .steps()
                .iter()
                .map(|step| step.name().map(|value| value.as_str()))
                .collect::<Vec<_>>(),
            vec![Some("second"), Some("first"), Some("third")]
        );
    }

    #[test]
    fn unsupported_large_fields_have_no_representation_in_the_output_model() {
        let trace = project_fixture(json!({
            "workflow_job": {
                "id": 41,
                "run_id": 31,
                "run_attempt": 2,
                "completed_at": "2026-08-06T10:05:00Z",
                "html_url": "https://example.invalid/jobs/41",
                "logs_url": "https://example.invalid/jobs/41/logs",
                "runner_url": "https://example.invalid/runners/1",
                "actor": {"login": "secret-user"},
                "steps": [{
                    "number": 1,
                    "completed_at": "2026-08-06T10:05:00Z",
                    "output": "secret output",
                    "logs": "secret logs",
                    "commands": ["super-secret-command"]
                }]
            }
        }))
        .expect("projection ignores unsupported fields");

        let rendered = format!("{trace:?}");
        for secret in [
            "https://example.invalid/jobs/41",
            "https://example.invalid/jobs/41/logs",
            "https://example.invalid/runners/1",
            "secret-user",
            "secret output",
            "secret logs",
            "super-secret-command",
        ] {
            assert!(!rendered.contains(secret), "trace debug leaked {secret}");
        }
    }

    #[test]
    fn malformed_or_non_array_steps_reject_projection() {
        assert!(project_fixture(json!({
            "workflow_job": {
                "id": 41,
                "run_id": 31,
                "run_attempt": 2,
                "completed_at": "2026-08-06T10:05:00Z",
                "steps": {}
            }
        }))
        .is_none());
        assert!(project_fixture(json!({
            "workflow_job": {
                "id": 41,
                "run_id": 31,
                "run_attempt": 2,
                "completed_at": "2026-08-06T10:05:00Z",
                "steps": [{"name": "missing number"}]
            }
        }))
        .is_none());
    }

    #[test]
    fn pre_epoch_receipt_times_project_with_checked_fallback() {
        let trace = project_fixture_at(
            json!({
                "workflow_job": {
                    "id": 41,
                    "run_id": 31,
                    "run_attempt": 2,
                    "steps": []
                }
            }),
            datetime!(1969-12-31 23:59:59.500 UTC),
        )
        .expect("pre-epoch receipt time remains representable");

        let expected = SystemTime::UNIX_EPOCH - Duration::from_millis(500);
        assert_eq!(trace.timing().source(), TimingSource::Fallback);
        assert_eq!(trace.timing().start(), expected);
        assert_eq!(trace.timing().end(), expected);
    }
}
