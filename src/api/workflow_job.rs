#[cfg(test)]
use std::cell::Cell;
use std::{
    fmt,
    time::{Duration, SystemTime},
};

use serde::{
    de::{Error as _, IgnoredAny, SeqAccess, Visitor},
    Deserialize, Deserializer,
};
use serde_json::Value;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use crate::{
    costs::CostsConfig,
    domain::{delivery::DeliveryId, merge_queue::PullRequestNumber},
    security::CanonicalRepositoryName,
    telemetry::{
        trace::CommitSha,
        workflow::{
            DisplayName, HistoricalTiming, RequiredChecks, TimingSource, WorkflowConclusion,
            WorkflowJobBilling, WorkflowJobCost, WorkflowJobId, WorkflowJobTrace,
            WorkflowJobTraceParts, WorkflowPullRequests, WorkflowRunAttempt, WorkflowRunContext,
            WorkflowRunId, WorkflowStepTrace, MAX_RUNNER_LABEL_COUNT,
        },
    },
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StepCount(usize);

#[cfg(test)]
thread_local! {
    static STEP_COUNT_DESERIALIZE_CALLS: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
fn increment_step_count_deserialize_calls() {
    STEP_COUNT_DESERIALIZE_CALLS.with(|calls| calls.set(calls.get() + 1));
}

#[cfg(test)]
fn step_count_deserialize_calls() -> usize {
    STEP_COUNT_DESERIALIZE_CALLS.with(Cell::get)
}

impl<'de> Deserialize<'de> for StepCount {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[cfg(test)]
        increment_step_count_deserialize_calls();

        struct StepCountVisitor;

        impl<'de> Visitor<'de> for StepCountVisitor {
            type Value = StepCount;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("an array of workflow steps")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut count = 0usize;
                while sequence.next_element::<IgnoredAny>()?.is_some() {
                    count = count
                        .checked_add(1)
                        .ok_or_else(|| A::Error::custom("workflow step count overflow"))?;
                }
                Ok(StepCount(count))
            }
        }

        deserializer.deserialize_seq(StepCountVisitor)
    }
}

#[derive(Deserialize)]
struct WorkflowJobAdmissionEnvelope {
    workflow_job: WorkflowJobAdmissionProjection,
}

#[derive(Deserialize)]
struct WorkflowJobAdmissionProjection {
    id: i64,
    run_id: i64,
    run_attempt: i64,
    #[serde(default)]
    steps: StepCount,
}

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
    labels: Option<Value>,
    runner_group_name: Option<Value>,
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

/// Admission-only view of a completed workflow-job payload.
///
/// This bounded representation retains only validated identifiers plus the counted number of step
/// entries, making retention of step payloads unrepresentable.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct WorkflowJobAdmission {
    run_id: WorkflowRunId,
    run_attempt: WorkflowRunAttempt,
    job_id: WorkflowJobId,
    step_count: usize,
}

impl WorkflowJobAdmission {
    /// Returns the validated workflow run identifier retained by this admission view.
    pub(super) fn run_id(&self) -> WorkflowRunId {
        self.run_id
    }

    /// Returns the validated workflow run attempt retained by this admission view.
    pub(super) fn run_attempt(&self) -> WorkflowRunAttempt {
        self.run_attempt
    }

    /// Returns the validated workflow job identifier retained by this admission view.
    pub(super) fn job_id(&self) -> WorkflowJobId {
        self.job_id
    }

    /// Returns the number of reported workflow-job steps counted during admission.
    pub(super) fn step_count(&self) -> usize {
        self.step_count
    }
}

impl fmt::Debug for WorkflowJobAdmission {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("WorkflowJobAdmission")
            .field("run_id", &"<redacted>")
            .field("run_attempt", &"<redacted>")
            .field("job_id", &"<redacted>")
            .field("step_count", &self.step_count)
            .finish()
    }
}

/// Inspects a completed workflow-job payload without retaining any individual step elements.
///
/// # Parameters
///
/// * `body` - The authenticated JSON request body.
///
/// # Returns
///
/// A bounded admission view containing validated positive identifiers and the counted number of
/// reported steps. Returns `None` when the payload wrapper is malformed, any required identifier is
/// non-positive, or `steps` is present with a non-array value.
pub(super) fn inspect_completed_job(body: &[u8]) -> Option<WorkflowJobAdmission> {
    let envelope: WorkflowJobAdmissionEnvelope = serde_json::from_slice(body).ok()?;

    Some(WorkflowJobAdmission {
        run_id: WorkflowRunId::new(envelope.workflow_job.run_id).ok()?,
        run_attempt: WorkflowRunAttempt::new(envelope.workflow_job.run_attempt).ok()?,
        job_id: WorkflowJobId::new(envelope.workflow_job.id).ok()?,
        step_count: envelope.workflow_job.steps.0,
    })
}

/// Projects one authenticated completed workflow-job payload into the bounded workflow model.
///
/// # Parameters
///
/// * `body` - The authenticated JSON request body.
/// * `repository_name` - The canonical authenticated repository name.
/// * `delivery_id` - The authenticated GitHub delivery identifier.
/// * `received_at` - The bounded webhook receipt timestamp.
/// * `workflow_run_context` - The correlated workflow-run metadata, when one was persisted.
/// * `required_checks` - The fresh branch-protection required checks of the run's target branch,
///   or `None` when the cache held no confident answer.
/// * `costs` - The price table, or `None` when cost attribution is off.
///
/// # Returns
///
/// A bounded workflow job trace when the wrapper and required workflow-job identifiers are valid;
/// otherwise `None`.
pub(crate) fn project_completed_job(
    body: &[u8],
    repository_name: &CanonicalRepositoryName,
    delivery_id: &DeliveryId,
    received_at: OffsetDateTime,
    workflow_run_context: Option<WorkflowRunContext>,
    required_checks: Option<&RequiredChecks>,
    costs: Option<&CostsConfig>,
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
        labels,
        runner_group_name,
        pull_requests,
        steps,
    } = envelope.workflow_job;

    let run_id = WorkflowRunId::new(run_id).ok()?;
    let run_attempt = WorkflowRunAttempt::new(run_attempt).ok()?;
    let job_id = WorkflowJobId::new(id).ok()?;
    let workflow_run_context = workflow_run_context
        .filter(|context| context.run_id() == run_id && context.run_attempt() == run_attempt);
    let timing = select_job_timing(
        parse_timestamp(started_at.as_ref()),
        parse_timestamp(completed_at.as_ref()),
        received_at,
    );
    let mut projected_steps = Vec::with_capacity(steps.len());
    for step in steps {
        let step_timing = select_step_timing(
            parse_timestamp(step.started_at.as_ref()),
            parse_timestamp(step.completed_at.as_ref()),
            &timing,
        );
        projected_steps.push(
            WorkflowStepTrace::new(
                step.number,
                sanitize_display_name(step.name.as_ref()),
                normalize_conclusion(step.conclusion.as_ref()),
                step_timing,
            )
            .ok()?,
        );
    }

    let conclusion = normalize_conclusion(conclusion.as_ref());
    let billing = project_billing(
        labels.as_ref(),
        runner_group_name.as_ref(),
        conclusion,
        &timing,
        costs,
    );
    let job_name = sanitize_display_name(name.as_ref());
    // "Required" is keyed by check name, and for GitHub Actions the check name is the job name.
    // An unnamed job can never match a required check, so it stays unknown rather than false.
    let required = required_checks
        .zip(job_name.as_ref())
        .map(|(checks, name)| checks.contains(name.as_str()));

    Some(
        WorkflowJobTrace::new(WorkflowJobTraceParts {
            repository_name: repository_name.clone(),
            delivery_id: *delivery_id,
            workflow_name: sanitize_display_name(workflow_name.as_ref()),
            run_id,
            run_attempt,
            job_id,
            job_name,
            conclusion,
            head_sha: parse_commit_sha(head_sha.as_ref()),
            pull_requests: WorkflowPullRequests::new(
                pull_requests
                    .iter()
                    .filter_map(|value| PullRequestNumber::new(value.number).ok())
                    .take(20),
            ),
            workflow_run_context,
            required,
            timing,
            steps: projected_steps,
        })
        .with_billing(billing),
    )
}

fn project_billing(
    labels: Option<&Value>,
    runner_group: Option<&Value>,
    conclusion: WorkflowConclusion,
    timing: &HistoricalTiming,
    costs: Option<&CostsConfig>,
) -> WorkflowJobBilling {
    let runner_labels: Vec<DisplayName> = labels
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .filter_map(DisplayName::sanitize)
        .take(MAX_RUNNER_LABEL_COUNT)
        .collect();
    let billable_minutes = billable_minutes(conclusion, timing);
    let cost = costs.map(|costs| {
        // A job that billed nothing has no rate to look up, which also keeps skipped jobs, whose
        // labels are often empty, from logging `ci price missing`.
        let (usd_per_minute, fell_back) = if billable_minutes == 0 {
            (0.0, false)
        } else {
            let labels: Vec<&str> = runner_labels.iter().map(DisplayName::as_str).collect();
            let rate = costs.runner_rate(&labels);
            (rate.usd_per_minute.usd(), rate.fell_back)
        };
        WorkflowJobCost {
            cost_usd: billable_minutes as f64 * usd_per_minute,
            price_table_version: costs.version().to_owned(),
            fell_back,
        }
    });

    WorkflowJobBilling {
        runner_labels,
        runner_group: sanitize_display_name(runner_group),
        billable_minutes,
        cost,
    }
}

/// Returns the whole minutes GitHub bills for a job: its run time rounded up to a minute.
///
/// Skipped jobs and jobs without reported start and end times never ran on a runner, so they
/// bill nothing.
fn billable_minutes(conclusion: WorkflowConclusion, timing: &HistoricalTiming) -> i64 {
    if conclusion == WorkflowConclusion::Skipped || timing.source() != TimingSource::Reported {
        return 0;
    }
    let nanos = timing
        .end()
        .duration_since(timing.start())
        .unwrap_or_default()
        .as_nanos();
    let minutes = nanos.div_ceil(60 * 1_000_000_000);
    i64::try_from(minutes).unwrap_or(i64::MAX)
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

    use super::{
        inspect_completed_job, offset_datetime_to_system_time, project_completed_job,
        step_count_deserialize_calls, WorkflowJobAdmission,
    };
    use crate::{
        costs::CostsConfig,
        telemetry::workflow::{WorkflowJobCost, WorkflowJobTrace, MAX_RUNNER_LABEL_COUNT},
    };
    use crate::{
        domain::delivery::DeliveryId,
        security::CanonicalRepositoryName,
        telemetry::workflow::{
            DisplayName, RequiredChecks, TimingSource, WorkflowBranch, WorkflowConclusion,
            WorkflowEvent, WorkflowRunAttempt, WorkflowRunContext, WorkflowRunId,
        },
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
        project_completed_job(
            &bytes,
            &repository_name(),
            &delivery_id(),
            received_at,
            None,
            None,
            None,
        )
    }

    fn project_fixture(body: Value) -> Option<crate::telemetry::workflow::WorkflowJobTrace> {
        project_fixture_at(body, datetime!(2026-08-06 10:06:00 UTC))
    }

    fn project_fixture_with_required_checks(
        body: Value,
        required_checks: Option<&RequiredChecks>,
    ) -> Option<crate::telemetry::workflow::WorkflowJobTrace> {
        let bytes = serde_json::to_vec(&body).expect("fixture JSON serializes");
        project_completed_job(
            &bytes,
            &repository_name(),
            &delivery_id(),
            datetime!(2026-08-06 10:06:00 UTC),
            None,
            required_checks,
            None,
        )
    }

    fn required_checks(names: &[&str]) -> RequiredChecks {
        RequiredChecks::new(names.iter().filter_map(|name| DisplayName::sanitize(name)))
    }

    fn named_job(name: Option<&str>) -> Value {
        let mut workflow_job = json!({
            "id": 41,
            "run_id": 31,
            "run_attempt": 1,
            "conclusion": "success",
            "steps": [],
        });
        if let Some(name) = name {
            workflow_job["name"] = json!(name);
        }
        json!({ "workflow_job": workflow_job })
    }

    #[test]
    fn a_job_named_by_a_cached_required_check_is_projected_as_required() {
        let trace = project_fixture_with_required_checks(
            named_job(Some("Linux Job")),
            Some(&required_checks(&["Linux Job", "macOS Job"])),
        )
        .expect("fixture projects");

        assert_eq!(trace.required(), Some(true));
    }

    #[test]
    fn a_job_absent_from_a_cached_required_check_set_is_projected_as_not_required() {
        let trace = project_fixture_with_required_checks(
            named_job(Some("Lint")),
            Some(&required_checks(&["Linux Job"])),
        )
        .expect("fixture projects");

        assert_eq!(trace.required(), Some(false));
    }

    #[test]
    fn a_job_is_matched_against_the_sanitized_name_that_the_span_reports() {
        // The projected job name has its control characters stripped, and cached check names are
        // sanitized identically, so the two forms compare equal.
        let trace = project_fixture_with_required_checks(
            named_job(Some("Linux	Job")),
            Some(&required_checks(&["Linux	Job"])),
        )
        .expect("fixture projects");

        assert_eq!(trace.job_name().map(DisplayName::as_str), Some("LinuxJob"));
        assert_eq!(trace.required(), Some(true));
    }

    #[test]
    fn an_absent_cache_answer_leaves_the_required_status_unknown() {
        let trace = project_fixture_with_required_checks(named_job(Some("Linux Job")), None)
            .expect("fixture projects");

        assert_eq!(trace.required(), None);
    }

    #[test]
    fn an_unnamed_job_stays_unknown_rather_than_claiming_it_is_not_required() {
        let trace = project_fixture_with_required_checks(
            named_job(None),
            Some(&required_checks(&["Linux Job"])),
        )
        .expect("fixture projects");

        assert_eq!(trace.job_name(), None);
        assert_eq!(trace.required(), None);
    }

    fn render_admission(admission: &WorkflowJobAdmission) -> String {
        format!("{admission:?}")
    }

    #[test]
    fn admission_counts_steps_and_validates_identifiers() {
        let body = serde_json::to_vec(&json!({
            "workflow_job": {
                "id": 41,
                "run_id": 31,
                "run_attempt": 2,
                "steps": [{"secret": "first"}, {"secret": "second"}]
            }
        }))
        .expect("fixture serializes");

        let admission = inspect_completed_job(&body).expect("admission is structurally valid");

        assert_eq!(admission.run_id().get(), 31);
        assert_eq!(admission.run_attempt().get(), 2);
        assert_eq!(admission.job_id().get(), 41);
        assert_eq!(admission.step_count(), 2);
        assert!(!render_admission(&admission).contains("first"));
    }

    #[test]
    fn admission_rejects_malformed_json_bytes() {
        assert!(inspect_completed_job(b"{").is_none());
    }

    #[test]
    fn admission_rejects_missing_workflow_job() {
        let body = serde_json::to_vec(&json!({})).expect("fixture serializes");

        assert!(inspect_completed_job(&body).is_none());
    }

    #[test]
    fn admission_rejects_null_workflow_job() {
        let body = serde_json::to_vec(&json!({"workflow_job": null})).expect("fixture serializes");

        assert!(inspect_completed_job(&body).is_none());
    }

    #[test]
    fn admission_rejects_wrong_type_workflow_job_envelopes() {
        for workflow_job in [json!("wrong type"), json!([]), json!(42)] {
            let body = serde_json::to_vec(&json!({"workflow_job": workflow_job}))
                .expect("fixture serializes");

            assert!(
                inspect_completed_job(&body).is_none(),
                "wrong-type workflow_job envelope was admitted: {body:?}"
            );
        }
    }

    #[test]
    fn admission_missing_steps_defaults_to_zero() {
        let body = serde_json::to_vec(&json!({
            "workflow_job": {
                "id": 41,
                "run_id": 31,
                "run_attempt": 2
            }
        }))
        .expect("fixture serializes");

        let admission = inspect_completed_job(&body).expect("missing steps defaults to zero");

        assert_eq!(admission.step_count(), 0);
    }

    #[test]
    fn admission_rejects_non_array_steps_and_invalid_required_ids() {
        let non_array_steps = serde_json::to_vec(&json!({
            "workflow_job": {
                "id": 41,
                "run_id": 31,
                "run_attempt": 2,
                "steps": {}
            }
        }))
        .expect("fixture serializes");
        assert!(inspect_completed_job(&non_array_steps).is_none());

        for (run_id, run_attempt, job_id) in [(0, 2, 41), (31, 0, 41), (31, 2, 0)] {
            let body = serde_json::to_vec(&json!({
                "workflow_job": {
                    "id": job_id,
                    "run_id": run_id,
                    "run_attempt": run_attempt,
                    "steps": []
                }
            }))
            .expect("fixture serializes");
            assert!(
                inspect_completed_job(&body).is_none(),
                "accepted invalid ids run_id={run_id} run_attempt={run_attempt} job_id={job_id}"
            );
        }
    }

    #[test]
    fn admission_counts_large_step_arrays_without_debug_leaks() {
        let secret = "forbidden step payload";
        let steps = (0..2_048)
            .map(|index| json!({"index": index, "secret": secret}))
            .collect::<Vec<_>>();
        let body = serde_json::to_vec(&json!({
            "workflow_job": {
                "id": 41,
                "run_id": 31,
                "run_attempt": 2,
                "steps": steps
            }
        }))
        .expect("fixture serializes");

        let admission = inspect_completed_job(&body).expect("large step array is counted");

        assert_eq!(admission.step_count(), 2_048);
        assert!(!render_admission(&admission).contains(secret));
    }

    #[test]
    fn detailed_projection_does_not_run_admission_deserialization() {
        let admission_deserialize_calls_before = step_count_deserialize_calls();

        let trace = project_fixture(json!({
            "workflow_job": {
                "id": 41,
                "run_id": 31,
                "run_attempt": 2,
                "completed_at": "2026-08-06T10:05:00Z",
                "steps": [{"number": 1}]
            }
        }))
        .expect("valid completed job projects");

        assert_eq!(trace.steps().len(), 1);
        assert_eq!(
            step_count_deserialize_calls(),
            admission_deserialize_calls_before
        );
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
    fn completed_projection_retains_matching_workflow_run_context() {
        let body = serde_json::to_vec(&json!({
            "workflow_job": {
                "id": 41,
                "run_id": 31,
                "run_attempt": 2,
                "completed_at": "2026-08-06T10:05:00Z",
                "steps": []
            }
        }))
        .expect("fixture serializes");
        let context = WorkflowRunContext::new(
            WorkflowRunId::new(31).expect("run id is positive"),
            WorkflowRunAttempt::new(2).expect("attempt is positive"),
            WorkflowEvent::MergeGroup,
            WorkflowBranch::sanitize("gh-readonly-queue/main/pr-7"),
            WorkflowBranch::sanitize("main"),
        );

        let trace = project_completed_job(
            &body,
            &repository_name(),
            &delivery_id(),
            datetime!(2026-08-06 10:06:00 UTC),
            Some(context),
            None,
            None,
        )
        .expect("valid completed job projects");

        let retained = trace.workflow_run_context().expect("context is retained");
        assert_eq!(retained.event(), WorkflowEvent::MergeGroup);
        assert_eq!(
            retained.source_branch().map(|branch| branch.as_str()),
            Some("gh-readonly-queue/main/pr-7")
        );
        assert_eq!(
            retained.target_branch().map(|branch| branch.as_str()),
            Some("main")
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
    fn non_positive_step_numbers_reject_entire_projection() {
        for invalid_step_number in [0, -1] {
            assert!(
                project_fixture(json!({
                    "workflow_job": {
                        "id": 41,
                        "run_id": 31,
                        "run_attempt": 2,
                        "completed_at": "2026-08-06T10:05:00Z",
                        "steps": [
                            {"number": 1},
                            {"number": invalid_step_number},
                            {"number": 2}
                        ]
                    }
                }))
                .is_none(),
                "accepted invalid step number {invalid_step_number}"
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

    const COSTS: &str = r#"{
        "runners": {
            "default_usd_per_minute": 0.01,
            "labels": {"ubuntu-24.04-xl": 0.08}
        }
    }"#;

    fn priced_job(workflow_job: Value, costs: Option<&CostsConfig>) -> WorkflowJobTrace {
        let body = serde_json::to_vec(&json!({ "workflow_job": workflow_job }))
            .expect("fixture serializes");
        project_completed_job(
            &body,
            &repository_name(),
            &delivery_id(),
            datetime!(2026-08-06 10:06:00 UTC),
            None,
            None,
            costs,
        )
        .expect("fixture projects")
    }

    fn timed_job(conclusion: &str, labels: Value) -> Value {
        json!({
            "id": 41,
            "run_id": 31,
            "run_attempt": 1,
            "conclusion": conclusion,
            "started_at": "2026-08-06T10:00:00Z",
            "completed_at": "2026-08-06T10:05:00.5Z",
            "labels": labels,
            "runner_group_name": "Larger\nrunners",
            "steps": []
        })
    }

    #[test]
    fn a_job_bills_its_run_time_rounded_up_to_whole_minutes() {
        let trace = priced_job(timed_job("success", json!(["ubuntu-24.04-xl"])), None);

        let billing = trace.billing().expect("billing is projected");
        assert_eq!(billing.billable_minutes, 6);
        assert_eq!(
            billing
                .runner_labels
                .iter()
                .map(DisplayName::as_str)
                .collect::<Vec<_>>(),
            ["ubuntu-24.04-xl"]
        );
        assert_eq!(
            billing.runner_group.as_ref().map(DisplayName::as_str),
            Some("Largerrunners")
        );
        assert_eq!(billing.cost, None);
    }

    #[test]
    fn skipped_and_untimed_jobs_bill_nothing() {
        let skipped = priced_job(timed_job("skipped", json!([])), None);
        let mut untimed = timed_job("cancelled", json!([]));
        untimed["started_at"] = Value::Null;
        let untimed = priced_job(untimed, None);

        for trace in [skipped, untimed] {
            assert_eq!(
                trace.billing().map(|billing| billing.billable_minutes),
                Some(0)
            );
        }
    }

    #[test]
    fn a_configured_table_prices_the_job_by_its_first_priced_label() {
        let costs = CostsConfig::from_json(COSTS).expect("costs are valid");

        let priced = priced_job(
            timed_job("failure", json!(["self-hosted", "Ubuntu-24.04-XL"])),
            Some(&costs),
        );
        let fallback = priced_job(timed_job("success", json!(["unknown"])), Some(&costs));

        let cost = priced.billing().and_then(|billing| billing.cost.clone());
        assert_eq!(
            cost,
            Some(WorkflowJobCost {
                cost_usd: 6.0 * 0.08,
                price_table_version: costs.version().to_owned(),
                fell_back: false,
            })
        );
        let cost = fallback.billing().and_then(|billing| billing.cost.clone());
        assert_eq!(cost.as_ref().map(|cost| cost.cost_usd), Some(6.0 * 0.01));
        assert_eq!(cost.map(|cost| cost.fell_back), Some(true));
    }

    #[test]
    fn a_job_that_billed_nothing_is_priced_at_zero_without_a_rate_lookup() {
        let costs = CostsConfig::from_json(COSTS).expect("costs are valid");

        let trace = priced_job(timed_job("skipped", json!([])), Some(&costs));

        let cost = trace.billing().and_then(|billing| billing.cost.clone());
        assert_eq!(cost.as_ref().map(|cost| cost.cost_usd), Some(0.0));
        assert_eq!(cost.map(|cost| cost.fell_back), Some(false));
    }

    #[test]
    fn runner_labels_are_sanitized_bounded_and_tolerate_bad_shapes() {
        let many: Vec<String> = (0..20).map(|index| format!("label-{index}")).collect();
        let bounded = priced_job(timed_job("success", json!(many)), None);
        let mixed = priced_job(timed_job("success", json!(["a\tb", 7, null, ""])), None);
        let not_an_array = priced_job(timed_job("success", json!("ubuntu-latest")), None);

        let labels = |trace: &WorkflowJobTrace| {
            trace
                .billing()
                .map(|billing| {
                    billing
                        .runner_labels
                        .iter()
                        .map(|label| label.as_str().to_owned())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        };
        assert_eq!(labels(&bounded).len(), MAX_RUNNER_LABEL_COUNT);
        assert_eq!(
            labels(&bounded).last().map(String::as_str),
            Some("label-15")
        );
        assert_eq!(labels(&mixed), ["ab"]);
        assert!(labels(&not_an_array).is_empty());
    }
}
