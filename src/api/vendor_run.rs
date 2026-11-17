use std::time::SystemTime;

use serde::Deserialize;
use serde_json::Value;

use crate::{
    api::workflow_run::unique_pull_request_number,
    costs::{CostsConfig, VendorRate},
    domain::merge_queue::PullRequestNumber,
    metrics::{Action, EventType},
    security::CanonicalRepositoryName,
    telemetry::{
        trace::CommitSha,
        vendor::{VendorRun, VendorTrigger},
    },
};

#[derive(Deserialize)]
struct CheckSuiteEnvelope {
    check_suite: CheckSuiteProjection,
}

#[derive(Deserialize)]
struct CheckSuiteProjection {
    head_sha: Option<Value>,
    app: Option<AppProjection>,
    pull_requests: Option<Value>,
}

#[derive(Deserialize)]
struct AppProjection {
    slug: Option<Value>,
}

#[derive(Deserialize)]
struct ReviewEnvelope {
    review: ReviewProjection,
    pull_request: ReviewPullRequestProjection,
}

#[derive(Deserialize)]
struct ReviewProjection {
    user: Option<UserProjection>,
}

#[derive(Deserialize)]
struct UserProjection {
    login: Option<Value>,
}

#[derive(Deserialize)]
struct ReviewPullRequestProjection {
    number: Option<i64>,
    head: Option<HeadProjection>,
}

#[derive(Deserialize)]
struct HeadProjection {
    sha: Option<Value>,
}

/// The fields of a vendor delivery that identify and attribute the run.
struct VendorRunFields<'costs> {
    vendor: VendorRate<'costs>,
    pull_request: Option<PullRequestNumber>,
    head_sha: Option<CommitSha>,
}

/// Returns the vendor trigger for an event and action pair, or `None` when it cannot bill a run.
pub(super) fn vendor_trigger(event_type: EventType, action: Action) -> Option<VendorTrigger> {
    match (event_type, action) {
        (EventType::CheckSuite, Action::Completed) => Some(VendorTrigger::CheckSuite),
        (EventType::PullRequestReview, Action::Submitted) => Some(VendorTrigger::PullRequestReview),
        _ => None,
    }
}

/// Projects and prices one vendor run from an authenticated delivery.
///
/// # Parameters
///
/// * `body` - The authenticated JSON request body.
/// * `trigger` - The trigger returned by [`vendor_trigger`] for this delivery.
/// * `costs` - The price table whose `vendors` are matched.
/// * `repository_name` - The canonical authenticated repository name.
/// * `received_at` - When the delivery arrived, used as the run's instant.
///
/// # Returns
///
/// The priced run when the check-suite app slug or reviewer login matches a configured vendor,
/// ignoring ASCII case. `None` for a malformed payload, a missing key, or no match.
pub(super) fn project_vendor_run(
    body: &[u8],
    trigger: VendorTrigger,
    costs: &CostsConfig,
    repository_name: &CanonicalRepositoryName,
    received_at: SystemTime,
) -> Option<VendorRun> {
    let fields = match trigger {
        VendorTrigger::CheckSuite => check_suite_fields(body, costs)?,
        VendorTrigger::PullRequestReview => review_fields(body, costs)?,
    };
    Some(VendorRun {
        repository_name: repository_name.clone(),
        vendor: fields.vendor.name.clone(),
        trigger,
        cost_usd: fields.vendor.usd_per_run.usd(),
        price_table_version: costs.version().to_owned(),
        pull_request: fields.pull_request,
        head_sha: fields.head_sha,
        received_at,
    })
}

fn check_suite_fields<'costs>(
    body: &[u8],
    costs: &'costs CostsConfig,
) -> Option<VendorRunFields<'costs>> {
    let check_suite = serde_json::from_slice::<CheckSuiteEnvelope>(body)
        .ok()?
        .check_suite;
    let slug = check_suite.app?.slug?;
    let vendor = costs.vendor_for_check_suite_app(slug.as_str()?)?;
    Some(VendorRunFields {
        vendor,
        // A suite on a fork names no PR, and one on a shared head can name several; neither can
        // be charged to a single PR.
        pull_request: unique_pull_request_number(
            check_suite
                .pull_requests
                .as_ref()
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|pull_request| pull_request.get("number")?.as_i64()),
        ),
        head_sha: parse_sha(check_suite.head_sha.as_ref()),
    })
}

fn review_fields<'costs>(
    body: &[u8],
    costs: &'costs CostsConfig,
) -> Option<VendorRunFields<'costs>> {
    let envelope = serde_json::from_slice::<ReviewEnvelope>(body).ok()?;
    let login = envelope.review.user?.login?;
    let vendor = costs.vendor_for_review_login(login.as_str()?)?;
    let pull_request = envelope.pull_request;
    Some(VendorRunFields {
        vendor,
        pull_request: unique_pull_request_number(pull_request.number),
        head_sha: parse_sha(pull_request.head.and_then(|head| head.sha).as_ref()),
    })
}

fn parse_sha(value: Option<&Value>) -> Option<CommitSha> {
    value
        .and_then(Value::as_str)
        .and_then(|sha| CommitSha::parse(sha).ok())
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use serde_json::{json, Value};

    use super::{project_vendor_run, vendor_trigger};
    use crate::{
        costs::CostsConfig,
        metrics::{Action, EventType},
        security::CanonicalRepositoryName,
        telemetry::vendor::{VendorRun, VendorTrigger},
    };

    const HEAD_SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn costs() -> CostsConfig {
        CostsConfig::from_json(
            r#"{"runners": {"default_usd_per_minute": 0.01},
                "vendors": {
                    "example-review-bot": {"match": {"review_login": "Review-Bot[bot]"},
                                           "usd_per_run": 0.5},
                    "example-visual-tests": {"match": {"check_suite_app": "visual-tests"},
                                             "usd_per_run": 0.25}
                }}"#,
        )
        .expect("costs are valid")
    }

    fn project(trigger: VendorTrigger, payload: Value) -> Option<VendorRun> {
        let body = serde_json::to_vec(&payload).expect("payload serializes");
        project_vendor_run(
            &body,
            trigger,
            &costs(),
            &CanonicalRepositoryName::new("owner/repo").expect("name is valid"),
            SystemTime::UNIX_EPOCH,
        )
    }

    fn check_suite(slug: &str, pull_requests: Value) -> Value {
        json!({
            "action": "completed",
            "check_suite": {
                "head_sha": HEAD_SHA.to_uppercase(),
                "app": {"slug": slug},
                "pull_requests": pull_requests
            },
            "repository": {"full_name": "owner/repo"}
        })
    }

    fn review(login: &str, number: Value) -> Value {
        json!({
            "action": "submitted",
            "review": {"user": {"login": login}},
            "pull_request": {"number": number, "head": {"sha": HEAD_SHA}},
            "repository": {"full_name": "owner/repo"}
        })
    }

    #[test]
    fn only_completed_check_suites_and_submitted_reviews_can_bill() {
        assert_eq!(
            vendor_trigger(EventType::CheckSuite, Action::Completed),
            Some(VendorTrigger::CheckSuite)
        );
        assert_eq!(
            vendor_trigger(EventType::PullRequestReview, Action::Submitted),
            Some(VendorTrigger::PullRequestReview)
        );
        for (event_type, action) in [
            (EventType::CheckSuite, Action::Requested),
            (EventType::CheckSuite, Action::Rerequested),
            (EventType::PullRequestReview, Action::Edited),
            (EventType::PullRequestReview, Action::Other),
            (EventType::CheckRun, Action::Completed),
            (EventType::PullRequest, Action::Submitted),
        ] {
            assert_eq!(vendor_trigger(event_type, action), None, "{event_type:?}");
        }
    }

    #[test]
    fn a_check_suite_matches_its_app_slug_ignoring_case() {
        let run = project(
            VendorTrigger::CheckSuite,
            check_suite("Visual-Tests", json!([{"number": 7}, {"number": 7}])),
        )
        .expect("the app slug matches");

        assert_eq!(run.vendor.as_str(), "example-visual-tests");
        assert_eq!(run.trigger, VendorTrigger::CheckSuite);
        assert_eq!(run.cost_usd, 0.25);
        assert_eq!(run.price_table_version, costs().version());
        assert_eq!(run.pull_request.map(|number| number.get()), Some(7));
        assert_eq!(
            run.head_sha.as_ref().map(|sha| sha.as_str()),
            Some(HEAD_SHA)
        );
        assert_eq!(run.repository_name.as_str(), "owner/repo");
    }

    #[test]
    fn a_check_suite_without_exactly_one_pull_request_is_unattributed() {
        for pull_requests in [
            json!([]),
            json!(null),
            json!([{"number": 7}, {"number": 8}]),
            json!([{"number": 0}]),
        ] {
            let run = project(
                VendorTrigger::CheckSuite,
                check_suite("visual-tests", pull_requests.clone()),
            )
            .expect("the run is still priced");
            assert_eq!(run.pull_request, None, "{pull_requests}");
        }
    }

    #[test]
    fn a_check_suite_with_malformed_pull_request_entries_is_still_priced() {
        for (pull_requests, expected) in [
            (json!([{"number": "7"}, 3, null, {"number": 7.5}]), None),
            (json!([{"number": 7}, "7", {"number": -1}, {}]), Some(7)),
            (
                json!([{"number": 7}, {"number": "8"}, {"number": 7}]),
                Some(7),
            ),
            (json!([{"number": 7}, [8], {"number": 9}]), None),
            (json!("7"), None),
        ] {
            let run = project(
                VendorTrigger::CheckSuite,
                check_suite("visual-tests", pull_requests.clone()),
            )
            .expect("the run is still priced");
            assert_eq!(
                run.pull_request.map(|number| number.get()),
                expected,
                "{pull_requests}"
            );
        }
    }

    #[test]
    fn a_submitted_review_matches_its_reviewer_login_ignoring_case() {
        let run = project(
            VendorTrigger::PullRequestReview,
            review("review-bot[BOT]", json!(42)),
        )
        .expect("the login matches");

        assert_eq!(run.vendor.as_str(), "example-review-bot");
        assert_eq!(run.trigger, VendorTrigger::PullRequestReview);
        assert_eq!(run.cost_usd, 0.5);
        assert_eq!(run.pull_request.map(|number| number.get()), Some(42));
        assert_eq!(
            run.head_sha.as_ref().map(|sha| sha.as_str()),
            Some(HEAD_SHA)
        );
    }

    #[test]
    fn a_review_without_a_valid_pull_request_or_sha_is_still_priced() {
        let mut payload = review("review-bot[bot]", json!(null));
        payload["pull_request"]["head"]["sha"] = json!("not-a-sha");

        let run = project(VendorTrigger::PullRequestReview, payload).expect("the login matches");

        assert_eq!(run.pull_request, None);
        assert_eq!(run.head_sha, None);
    }

    #[test]
    fn unmatched_or_malformed_deliveries_are_not_vendor_runs() {
        let cases = [
            (
                VendorTrigger::CheckSuite,
                check_suite("github-actions", json!([])),
            ),
            (
                VendorTrigger::CheckSuite,
                check_suite("review-bot[bot]", json!([])),
            ),
            (
                VendorTrigger::PullRequestReview,
                review("visual-tests", json!(1)),
            ),
            (
                VendorTrigger::PullRequestReview,
                review("octocat", json!(1)),
            ),
            (
                VendorTrigger::CheckSuite,
                json!({"check_suite": {"head_sha": HEAD_SHA}}),
            ),
            (
                VendorTrigger::CheckSuite,
                json!({"check_suite": {"app": {"slug": 7}}}),
            ),
            (
                VendorTrigger::PullRequestReview,
                json!({"review": {"user": null}, "pull_request": {"number": 1}}),
            ),
            (
                VendorTrigger::PullRequestReview,
                json!({"review": {"user": {"login": "review-bot[bot]"}}}),
            ),
        ];
        for (trigger, payload) in cases {
            assert_eq!(project(trigger, payload.clone()), None, "{payload}");
        }
    }
}
