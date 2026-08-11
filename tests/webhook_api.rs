use std::{
    io::{self, Write},
    sync::{Arc, Mutex, Once},
};

use axum::{
    body::{to_bytes, Body},
    http::{header::CONTENT_TYPE, Request, StatusCode},
    response::Response,
    Router,
};
use github_webhook_exporter::{
    app::{build_router, AppState},
    config::DEFAULT_WORKFLOW_JOB_MAX_STEPS,
    security::{
        AdminAuthenticator, AdminToken, CanonicalRepositoryName, MasterKey, RepositorySecret,
        RepositorySecretCipher,
    },
    storage::{open_database, RepositoryStore},
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::{Row, SqlitePool};
use tower::ServiceExt;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::fmt::MakeWriter;

const SECRET: &str = "webhook-test-secret";
const DELIVERY_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
const PULL_REQUEST_SHA: &str = "0123456789abcdef0123456789abcdef01234567";
const PAYLOAD: &[u8] = br#"{"action":"opened","repository":{"full_name":"owner/repository"}}"#;
const MERGE_GROUP_CHECKS_REQUESTED: &[u8] =
    br#"{"action":"checks_requested","repository":{"full_name":"owner/repository"}}"#;
const MERGE_GROUP_DESTROYED_MERGED: &[u8] =
    br#"{"action":"destroyed","reason":"merged","repository":{"full_name":"owner/repository"}}"#;
const MERGE_GROUP_DESTROYED_DEQUEUED: &[u8] =
    br#"{"action":"destroyed","reason":"dequeued","repository":{"full_name":"owner/repository"}}"#;
const MERGE_GROUP_DESTROYED_INVALIDATED: &[u8] = br#"{"action":"destroyed","reason":"invalidated","repository":{"full_name":"owner/repository"}}"#;
const MERGE_GROUP_DESTROYED_OTHER: &[u8] =
    br#"{"action":"destroyed","reason":"unknown","repository":{"full_name":"owner/repository"}}"#;
const MERGE_GROUP_DESTROYED_EMPTY: &[u8] =
    br#"{"action":"destroyed","reason":"","repository":{"full_name":"owner/repository"}}"#;
const MERGE_GROUP_DESTROYED_MISSING: &[u8] =
    br#"{"action":"destroyed","repository":{"full_name":"owner/repository"}}"#;
const MERGE_GROUP_DESTROYED_NULL: &[u8] =
    br#"{"action":"destroyed","reason":null,"repository":{"full_name":"owner/repository"}}"#;
const MERGE_GROUP_DESTROYED_NUMBER: &[u8] =
    br#"{"action":"destroyed","reason":42,"repository":{"full_name":"owner/repository"}}"#;
const MERGE_GROUP_DESTROYED_MIXED_CASE: &[u8] =
    br#"{"action":"destroyed","reason":"Merged","repository":{"full_name":"owner/repository"}}"#;
const MERGE_GROUP_DESTROYED_MALICIOUS: &[u8] = br#"{"action":"destroyed","reason":"merged\\nsha256=secret-group-sha","merge_group":{"head_sha":"secret-group-sha"},"repository":{"full_name":"owner/repository"}}"#;
const MERGE_GROUP_UNSUPPORTED_ACTION: &[u8] =
    br#"{"action":"created","reason":"merged","repository":{"full_name":"owner/repository"}}"#;
const PULL_REQUEST_ENQUEUED: &[u8] = br#"{"action":"enqueued","pull_request":{"number":42,"updated_at":"2026-08-05T10:00:00Z","head":{"sha":"0123456789abcdef0123456789abcdef01234567"}},"repository":{"full_name":"owner/repository"}}"#;
const PULL_REQUEST_DEQUEUED: &[u8] = br#"{"action":"dequeued","reason":"malicious-raw-reason","pull_request":{"number":42,"updated_at":"2026-08-05T10:02:00Z","head":{"sha":"0123456789abcdef0123456789abcdef01234567"}},"repository":{"full_name":"owner/repository"}}"#;
const PULL_REQUEST_MERGED: &[u8] = br#"{"action":"closed","pull_request":{"number":42,"updated_at":"2026-08-05T10:03:00Z","merged":true,"head":{"sha":"0123456789abcdef0123456789abcdef01234567"}},"repository":{"full_name":"owner/repository"}}"#;
const PULL_REQUEST_UNMERGED: &[u8] = br#"{"action":"closed","pull_request":{"number":42,"updated_at":"2026-08-05T10:01:00Z","merged":false},"repository":{"full_name":"owner/repository"}}"#;
const PULL_REQUEST_MALFORMED_TIMESTAMP: &[u8] = br#"{"action":"enqueued","pull_request":{"number":42,"updated_at":"not-a-timestamp"},"repository":{"full_name":"owner/repository"}}"#;
const PULL_REQUEST_INVALID_NUMBER_TYPE: &[u8] = br#"{"action":"enqueued","pull_request":{"number":"42","updated_at":"2026-08-05T10:00:00Z"},"repository":{"full_name":"owner/repository"}}"#;
const PULL_REQUEST_NON_STRING_SHA: &[u8] = br#"{"action":"enqueued","pull_request":{"number":44,"updated_at":"2026-08-05T10:04:00Z","head":{"sha":42}},"repository":{"full_name":"owner/repository"}}"#;
const MERGE_GROUP_NON_STRING_SHA: &[u8] = br#"{"action":"checks_requested","merge_group":{"head_sha":{"unexpected":"value"}},"repository":{"full_name":"owner/repository"}}"#;
const PULL_REQUEST_ENQUEUED_FUTURE: &[u8] = br#"{"action":"enqueued","pull_request":{"number":43,"updated_at":"2027-08-06T10:00:00Z"},"repository":{"full_name":"owner/repository"}}"#;
const PULL_REQUEST_DEQUEUED_PAST: &[u8] = br#"{"action":"dequeued","pull_request":{"number":43,"updated_at":"2026-08-05T10:00:00Z"},"repository":{"full_name":"owner/repository"}}"#;
static TRACING_INIT: Once = Once::new();

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

struct TestApp {
    _directory: tempfile::TempDir,
    pool: SqlitePool,
    router: Router,
}

fn router_for_pool(pool: SqlitePool, body_limit: usize) -> Router {
    let cipher = RepositorySecretCipher::new(
        &MasterKey::from_slice(&[7_u8; 32]).expect("test master key is valid"),
    )
    .expect("repository cipher initializes");
    let admin_token = AdminToken::new("admin-token".to_owned()).expect("admin token is valid");
    build_router(AppState::new(
        RepositoryStore::new(pool, cipher),
        AdminAuthenticator::new(&admin_token),
        body_limit,
        DEFAULT_WORKFLOW_JOB_MAX_STEPS,
    ))
}

async fn test_app(body_limit: usize, enabled: Option<bool>) -> TestApp {
    let repositories = enabled.map_or_else(Vec::new, |enabled| {
        vec![("owner/repository", SECRET, enabled)]
    });
    test_app_with_repositories(body_limit, &repositories).await
}

async fn test_app_with_repositories(
    body_limit: usize,
    repositories: &[(&str, &str, bool)],
) -> TestApp {
    TRACING_INIT.call_once(|| {
        tracing::subscriber::set_global_default(tracing_subscriber::registry())
            .expect("test tracing registry initializes once");
    });
    let directory = tempfile::tempdir().expect("temporary directory is created");
    let pool = open_database(&directory.path().join("exporter.sqlite3"))
        .await
        .expect("database opens and migrates");
    let cipher = RepositorySecretCipher::new(
        &MasterKey::from_slice(&[7_u8; 32]).expect("test master key is valid"),
    )
    .expect("repository cipher initializes");
    let store = RepositoryStore::new(pool.clone(), cipher);
    for (repository_name, secret, enabled) in repositories {
        store
            .create(
                CanonicalRepositoryName::new(repository_name).expect("repository name is valid"),
                RepositorySecret::new((*secret).to_owned()).expect("secret is valid"),
                *enabled,
            )
            .await
            .expect("repository fixture is created");
    }
    let admin_token = AdminToken::new("admin-token".to_owned()).expect("admin token is valid");
    let state = AppState::new(
        store,
        AdminAuthenticator::new(&admin_token),
        body_limit,
        DEFAULT_WORKFLOW_JOB_MAX_STEPS,
    );

    TestApp {
        _directory: directory,
        pool,
        router: build_router(state),
    }
}

fn signature(secret: &str, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC key is valid");
    mac.update(body);
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

fn webhook_request(body: &[u8], repository_secret: &str, delivery_id: &str) -> Request<Body> {
    webhook_request_for_event(body, repository_secret, delivery_id, "pull_request")
}

fn webhook_request_for_event(
    body: &[u8],
    repository_secret: &str,
    delivery_id: &str,
    event_type: &str,
) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/webhooks/github")
        .header(CONTENT_TYPE, "application/json")
        .header("X-GitHub-Event", event_type)
        .header("X-GitHub-Delivery", delivery_id)
        .header("X-Hub-Signature-256", signature(repository_secret, body))
        .body(Body::from(body.to_vec()))
        .expect("webhook request is valid")
}

async fn response_body(response: Response) -> Vec<u8> {
    to_bytes(response.into_body(), 64 * 1_024)
        .await
        .expect("response body is readable")
        .to_vec()
}

async fn metrics(router: Router) -> String {
    let response = router
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .expect("metrics request is valid"),
        )
        .await
        .expect("metrics request succeeds");
    String::from_utf8(response_body(response).await).expect("metrics are UTF-8")
}

#[tokio::test]
async fn repository_scoped_metrics_distinguish_full_names() {
    const FIRST_SECRET: &str = "first-repository-secret";
    const SECOND_SECRET: &str = "second-repository-secret";
    let app = test_app_with_repositories(
        2_097_152,
        &[
            ("PeterGrace/GitHub-Webhook-Exporter", FIRST_SECRET, true),
            ("Other/Repository", SECOND_SECRET, true),
        ],
    )
    .await;
    let first_body =
        br#"{"action":"opened","repository":{"full_name":"PeterGrace/GitHub-Webhook-Exporter"}}"#;
    let second_body = br#"{"action":"opened","repository":{"full_name":"Other/Repository"}}"#;

    for (body, secret, delivery_id) in [
        (
            first_body.as_slice(),
            FIRST_SECRET,
            "650e8400-e29b-41d4-a716-446655440001",
        ),
        (
            second_body.as_slice(),
            SECOND_SECRET,
            "650e8400-e29b-41d4-a716-446655440002",
        ),
    ] {
        let response = app
            .router
            .clone()
            .oneshot(webhook_request(body, secret, delivery_id))
            .await
            .expect("webhook request succeeds");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    let exposition = metrics(app.router).await;
    assert!(exposition.contains(
        "github_webhook_requests_total{repository=\"petergrace/github-webhook-exporter\",result=\"accepted\"} 1"
    ));
    assert!(exposition.contains(
        "github_webhook_requests_total{repository=\"other/repository\",result=\"accepted\"} 1"
    ));
    assert!(exposition.contains(
        "github_webhook_events_total{repository=\"petergrace/github-webhook-exporter\",event_type=\"pull_request\",action=\"opened\"} 1"
    ));
    assert!(exposition.contains(
        "github_webhook_events_total{repository=\"other/repository\",event_type=\"pull_request\",action=\"opened\"} 1"
    ));
    assert!(!exposition.contains("repository=\"github-webhook-exporter\""));
}

#[tokio::test]
async fn pull_request_queue_enqueue_and_dequeue_commit_one_unknown_completion() {
    let app = test_app(2_097_152, Some(true)).await;

    for (body, delivery_id) in [
        (
            PULL_REQUEST_ENQUEUED,
            "550e8400-e29b-41d4-a716-446655440040",
        ),
        (
            PULL_REQUEST_DEQUEUED,
            "550e8400-e29b-41d4-a716-446655440041",
        ),
    ] {
        let response = app
            .router
            .clone()
            .oneshot(webhook_request(body, SECRET, delivery_id))
            .await
            .expect("pull-request webhook request succeeds");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    let row = sqlx::query(
        "SELECT enqueued_at, completed_at, outcome, reason_code FROM merge_queue_attempts",
    )
    .fetch_one(&app.pool)
    .await
    .expect("completed queue attempt is readable");
    assert_eq!(
        row.get::<String, _>("enqueued_at"),
        "2026-08-05T10:00:00.000Z"
    );
    assert_eq!(
        row.get::<String, _>("completed_at"),
        "2026-08-05T10:02:00.000Z"
    );
    assert_eq!(row.get::<String, _>("outcome"), "unknown");
    assert_eq!(row.get::<String, _>("reason_code"), "unclassified_dequeue");

    let exposition = metrics(app.router).await;
    for expected in [
        "github_merge_queue_pr_outcomes_total{repository=\"owner/repository\",outcome=\"unknown\",reason=\"unclassified_dequeue\"} 1",
        "github_merge_queue_attempt_duration_seconds_count{repository=\"owner/repository\",outcome=\"unknown\"} 1",
        "github_merge_queue_attempt_duration_seconds_sum{repository=\"owner/repository\",outcome=\"unknown\"} 120.0",
    ] {
        assert!(exposition.contains(expected), "missing {expected:?}");
    }
    assert!(!exposition.contains("malicious-raw-reason"));
}

#[tokio::test]
async fn pull_request_queue_replays_and_unmerged_close_are_idempotent() {
    let app = test_app(2_097_152, Some(true)).await;
    let cases = [
        (
            PULL_REQUEST_ENQUEUED,
            "550e8400-e29b-41d4-a716-446655440050",
        ),
        (
            PULL_REQUEST_ENQUEUED,
            "550e8400-e29b-41d4-a716-446655440051",
        ),
        (
            PULL_REQUEST_UNMERGED,
            "550e8400-e29b-41d4-a716-446655440052",
        ),
        (PULL_REQUEST_MERGED, "550e8400-e29b-41d4-a716-446655440053"),
        (PULL_REQUEST_MERGED, "550e8400-e29b-41d4-a716-446655440054"),
        (PULL_REQUEST_MERGED, "550e8400-e29b-41d4-a716-446655440054"),
    ];

    for (body, delivery_id) in cases {
        let response = app
            .router
            .clone()
            .oneshot(webhook_request(body, SECRET, delivery_id))
            .await
            .expect("pull-request webhook request succeeds");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    let row = sqlx::query(
        "SELECT COUNT(*) AS attempt_count, outcome, reason_code FROM merge_queue_attempts",
    )
    .fetch_one(&app.pool)
    .await
    .expect("queue attempt is readable");
    assert_eq!(row.get::<i64, _>("attempt_count"), 1);
    assert_eq!(row.get::<String, _>("outcome"), "succeeded");
    assert_eq!(row.get::<String, _>("reason_code"), "pull_request_merged");

    let exposition = metrics(app.router).await;
    for expected in [
        "github_merge_queue_pr_outcomes_total{repository=\"owner/repository\",outcome=\"succeeded\",reason=\"pull_request_merged\"} 1",
        "github_merge_queue_attempt_duration_seconds_count{repository=\"owner/repository\",outcome=\"succeeded\"} 1",
        "github_merge_queue_attempt_duration_seconds_sum{repository=\"owner/repository\",outcome=\"succeeded\"} 180.0",
        "github_webhook_duplicates_total{repository=\"owner/repository\"} 1",
    ] {
        assert!(exposition.contains(expected), "missing {expected:?}");
    }
}

#[tokio::test]
async fn pull_request_queue_attempt_completes_after_database_restart() {
    let directory = tempfile::tempdir().expect("temporary directory is created");
    let database_path = directory.path().join("queue-restart.sqlite3");
    let first_pool = open_database(&database_path)
        .await
        .expect("first database instance opens");
    let cipher = RepositorySecretCipher::new(
        &MasterKey::from_slice(&[7_u8; 32]).expect("test master key is valid"),
    )
    .expect("repository cipher initializes");
    RepositoryStore::new(first_pool.clone(), cipher)
        .create(
            CanonicalRepositoryName::new("owner/repository").expect("repository name is valid"),
            RepositorySecret::new(SECRET.to_owned()).expect("secret is valid"),
            true,
        )
        .await
        .expect("repository configuration is created");
    let first_router = router_for_pool(first_pool.clone(), 2_097_152);
    for _ in 0..2 {
        let enqueue_response = first_router
            .clone()
            .oneshot(webhook_request(
                PULL_REQUEST_ENQUEUED,
                SECRET,
                "550e8400-e29b-41d4-a716-446655440090",
            ))
            .await
            .expect("enqueue webhook succeeds");
        assert_eq!(enqueue_response.status(), StatusCode::NO_CONTENT);
    }
    let first_exposition = metrics(first_router).await;
    assert!(first_exposition
        .contains("github_webhook_duplicates_total{repository=\"owner/repository\"} 1"));
    first_pool.close().await;

    let second_pool = open_database(&database_path)
        .await
        .expect("second database instance opens");
    let second_router = router_for_pool(second_pool.clone(), 2_097_152);
    for _ in 0..2 {
        let completion_response = second_router
            .clone()
            .oneshot(webhook_request(
                PULL_REQUEST_MERGED,
                SECRET,
                "550e8400-e29b-41d4-a716-446655440091",
            ))
            .await
            .expect("completion webhook succeeds after restart");
        assert_eq!(completion_response.status(), StatusCode::NO_CONTENT);
    }

    let attempt: (i64, String) = sqlx::query_as(
        "SELECT COUNT(*), outcome FROM merge_queue_attempts WHERE pull_request_number = 42",
    )
    .fetch_one(&second_pool)
    .await
    .expect("completed attempt is readable after restart");
    assert_eq!(attempt, (1, "succeeded".to_owned()));
    let delivery_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM processed_deliveries")
        .fetch_one(&second_pool)
        .await
        .expect("durable delivery claims are countable");
    assert_eq!(delivery_count, 2);
    let exposition = metrics(second_router).await;
    for expected in [
        "github_merge_queue_pr_outcomes_total{repository=\"owner/repository\",outcome=\"succeeded\",reason=\"pull_request_merged\"} 1",
        "github_merge_queue_attempt_duration_seconds_count{repository=\"owner/repository\",outcome=\"succeeded\"} 1",
        "github_webhook_duplicates_total{repository=\"owner/repository\"} 1",
        "github_merge_group_events_total{repository=\"unknown\",action=\"destroyed\",reason=\"merged\"} 0",
    ] {
        assert!(exposition.contains(expected), "missing {expected:?}");
    }
}

#[tokio::test]
async fn concurrent_pull_request_queue_completions_record_one_outcome() {
    let app = test_app(2_097_152, Some(true)).await;
    let enqueue_response = app
        .router
        .clone()
        .oneshot(webhook_request(
            PULL_REQUEST_ENQUEUED,
            SECRET,
            "550e8400-e29b-41d4-a716-446655440090",
        ))
        .await
        .expect("enqueue webhook succeeds");
    assert_eq!(enqueue_response.status(), StatusCode::NO_CONTENT);
    let mut tasks = tokio::task::JoinSet::new();

    for index in 0..16 {
        let router = app.router.clone();
        tasks.spawn(async move {
            let delivery_id = format!("550e8400-e29b-41d4-a716-44665544{:04x}", 0x100 + index);
            router
                .oneshot(webhook_request(PULL_REQUEST_MERGED, SECRET, &delivery_id))
                .await
        });
    }

    while let Some(result) = tasks.join_next().await {
        let response = result
            .expect("completion task finishes")
            .expect("completion webhook succeeds");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }
    let exposition = metrics(app.router).await;
    for expected in [
        "github_merge_queue_pr_outcomes_total{repository=\"owner/repository\",outcome=\"succeeded\",reason=\"pull_request_merged\"} 1",
        "github_merge_queue_attempt_duration_seconds_count{repository=\"owner/repository\",outcome=\"succeeded\"} 1",
    ] {
        assert!(exposition.contains(expected), "missing {expected:?}");
    }
}

#[tokio::test]
async fn pull_request_queue_uses_receipt_fallback_and_rejects_malformed_typed_projection() {
    let app = test_app(2_097_152, Some(true)).await;
    let before = time::OffsetDateTime::now_utc();
    let fallback_response = app
        .router
        .clone()
        .oneshot(webhook_request(
            PULL_REQUEST_MALFORMED_TIMESTAMP,
            SECRET,
            "550e8400-e29b-41d4-a716-446655440060",
        ))
        .await
        .expect("fallback webhook request succeeds");
    let after = time::OffsetDateTime::now_utc();
    assert_eq!(fallback_response.status(), StatusCode::NO_CONTENT);
    let persisted: String = sqlx::query_scalar("SELECT enqueued_at FROM merge_queue_attempts")
        .fetch_one(&app.pool)
        .await
        .expect("fallback timestamp is readable");
    let persisted =
        time::OffsetDateTime::parse(&persisted, &time::format_description::well_known::Rfc3339)
            .expect("fallback timestamp is valid RFC 3339");
    assert!(persisted >= before - time::Duration::SECOND && persisted <= after);

    let malformed_response = app
        .router
        .oneshot(webhook_request(
            PULL_REQUEST_INVALID_NUMBER_TYPE,
            SECRET,
            "550e8400-e29b-41d4-a716-446655440061",
        ))
        .await
        .expect("malformed webhook request completes");
    assert_eq!(malformed_response.status(), StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pull_request_queue_missing_and_invalid_duration_use_only_failure_metrics() {
    let app = test_app(2_097_152, Some(true)).await;
    let cases = [
        (
            PULL_REQUEST_DEQUEUED,
            "550e8400-e29b-41d4-a716-446655440070",
        ),
        (
            PULL_REQUEST_ENQUEUED_FUTURE,
            "550e8400-e29b-41d4-a716-446655440071",
        ),
        (
            PULL_REQUEST_DEQUEUED_PAST,
            "550e8400-e29b-41d4-a716-446655440072",
        ),
    ];

    for (body, delivery_id) in cases {
        let response = app
            .router
            .clone()
            .oneshot(webhook_request(body, SECRET, delivery_id))
            .await
            .expect("pull-request webhook request succeeds");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    let exposition = metrics(app.router).await;
    for expected in [
        "github_merge_queue_transition_failures_total{repository=\"owner/repository\",reason=\"missing_active_attempt\"} 1",
        "github_merge_queue_transition_failures_total{repository=\"owner/repository\",reason=\"invalid_duration\"} 1",
        "github_merge_queue_pr_outcomes_total{repository=\"unknown\",outcome=\"unknown\",reason=\"unclassified_dequeue\"} 0",
        "github_merge_queue_attempt_duration_seconds_count{repository=\"unknown\",outcome=\"unknown\"} 0",
    ] {
        assert!(exposition.contains(expected), "missing {expected:?}");
    }
}

#[tokio::test]
async fn pull_request_queue_state_failure_is_redacted_observable_and_returns_no_content() {
    let captured_logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(captured_logs.clone())
        .finish();
    let app = test_app(2_097_152, Some(true)).await;
    let enqueue_response = app
        .router
        .clone()
        .oneshot(webhook_request(
            PULL_REQUEST_ENQUEUED,
            SECRET,
            "550e8400-e29b-41d4-a716-446655440080",
        ))
        .await
        .expect("enqueue webhook succeeds");
    assert_eq!(enqueue_response.status(), StatusCode::NO_CONTENT);
    sqlx::query(
        "CREATE TRIGGER reject_webhook_queue_completion BEFORE UPDATE ON merge_queue_attempts \
         BEGIN SELECT RAISE(ABORT, 'sensitive-queue-failure'); END",
    )
    .execute(&app.pool)
    .await
    .expect("queue failure trigger is installed");

    let response = app
        .router
        .clone()
        .oneshot(webhook_request(
            PULL_REQUEST_DEQUEUED,
            SECRET,
            "550e8400-e29b-41d4-a716-446655440081",
        ))
        .with_subscriber(subscriber)
        .await
        .expect("queue failure webhook completes");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let response_text =
        String::from_utf8(response_body(response).await).expect("response body is UTF-8");
    let row = sqlx::query("SELECT completed_at, outcome, reason_code FROM merge_queue_attempts")
        .fetch_one(&app.pool)
        .await
        .expect("pending attempt remains readable");
    assert_eq!(row.get::<Option<String>, _>("completed_at"), None);
    assert_eq!(row.get::<String, _>("outcome"), "pending");
    assert_eq!(row.get::<String, _>("reason_code"), "none");

    let replay_response = app
        .router
        .clone()
        .oneshot(webhook_request(
            PULL_REQUEST_DEQUEUED,
            SECRET,
            "550e8400-e29b-41d4-a716-446655440081",
        ))
        .await
        .expect("claimed queue failure replay completes");
    assert_eq!(replay_response.status(), StatusCode::NO_CONTENT);

    let exposition = metrics(app.router).await;
    assert!(
        exposition.contains("github_webhook_processing_failures_total{repository=\"owner/repository\",stage=\"queue_state\"} 1")
    );
    assert!(
        exposition.contains("github_webhook_duplicates_total{repository=\"owner/repository\"} 1")
    );
    assert!(exposition.contains(
        "github_merge_queue_pr_outcomes_total{repository=\"unknown\",outcome=\"unknown\",reason=\"unclassified_dequeue\"} 0"
    ));
    let logs = captured_logs.text();
    assert!(!response_text.contains("owner/repository"));
    assert!(!logs.contains("owner/repository"));
    assert_eq!(logs.matches("GitHub webhook processing failed").count(), 1);
    assert!(logs.contains("stage=\"queue_state\""));
    assert!(logs.contains("error_correlation_id="));
    for output in [response_text.as_str(), exposition.as_str(), logs.as_str()] {
        for forbidden in [
            "malicious-raw-reason",
            "sensitive-queue-failure",
            "550e8400-e29b-41d4-a716-446655440081",
            SECRET,
            PULL_REQUEST_SHA,
            "sha256=",
        ] {
            assert!(!output.contains(forbidden));
        }
    }
}

#[tokio::test]
async fn supported_merge_group_events_update_bounded_metrics_without_attempt_state() {
    let app = test_app(2_097_152, Some(true)).await;
    let cases = [
        (
            MERGE_GROUP_CHECKS_REQUESTED,
            "550e8400-e29b-41d4-a716-446655440001",
        ),
        (
            MERGE_GROUP_DESTROYED_MERGED,
            "550e8400-e29b-41d4-a716-446655440002",
        ),
        (
            MERGE_GROUP_DESTROYED_DEQUEUED,
            "550e8400-e29b-41d4-a716-446655440003",
        ),
        (
            MERGE_GROUP_DESTROYED_INVALIDATED,
            "550e8400-e29b-41d4-a716-446655440004",
        ),
        (
            MERGE_GROUP_DESTROYED_OTHER,
            "550e8400-e29b-41d4-a716-446655440005",
        ),
    ];

    for (body, delivery_id) in cases {
        let response = app
            .router
            .clone()
            .oneshot(webhook_request_for_event(
                body,
                SECRET,
                delivery_id,
                "merge_group",
            ))
            .await
            .expect("merge-group webhook request succeeds");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    let exposition = metrics(app.router).await;
    for expected in [
        "github_webhook_events_total{repository=\"owner/repository\",event_type=\"merge_group\",action=\"checks_requested\"} 1",
        "github_webhook_events_total{repository=\"owner/repository\",event_type=\"merge_group\",action=\"destroyed\"} 4",
        "github_merge_group_events_total{repository=\"owner/repository\",action=\"checks_requested\",reason=\"none\"} 1",
        "github_merge_group_events_total{repository=\"owner/repository\",action=\"destroyed\",reason=\"merged\"} 1",
        "github_merge_group_events_total{repository=\"owner/repository\",action=\"destroyed\",reason=\"dequeued\"} 1",
        "github_merge_group_events_total{repository=\"owner/repository\",action=\"destroyed\",reason=\"invalidated\"} 1",
        "github_merge_group_events_total{repository=\"owner/repository\",action=\"destroyed\",reason=\"other\"} 1",
    ] {
        assert!(exposition.contains(expected), "missing {expected:?}");
    }
    let attempt_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM merge_queue_attempts")
        .fetch_one(&app.pool)
        .await
        .expect("merge-queue attempt count is readable");
    assert_eq!(attempt_count, 0);
}

#[tokio::test]
async fn merge_group_destroyed_untrusted_reasons_collapse_to_other_without_disclosure() {
    let captured_logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(captured_logs.clone())
        .finish();
    let app = test_app(2_097_152, Some(true)).await;
    let malicious_signature = signature(SECRET, MERGE_GROUP_DESTROYED_MALICIOUS);
    let cases = [
        (
            MERGE_GROUP_DESTROYED_EMPTY,
            "550e8400-e29b-41d4-a716-446655440010",
        ),
        (
            MERGE_GROUP_DESTROYED_MISSING,
            "550e8400-e29b-41d4-a716-446655440011",
        ),
        (
            MERGE_GROUP_DESTROYED_NULL,
            "550e8400-e29b-41d4-a716-446655440012",
        ),
        (
            MERGE_GROUP_DESTROYED_NUMBER,
            "550e8400-e29b-41d4-a716-446655440013",
        ),
        (
            MERGE_GROUP_DESTROYED_MIXED_CASE,
            "550e8400-e29b-41d4-a716-446655440014",
        ),
        (
            MERGE_GROUP_DESTROYED_MALICIOUS,
            "550e8400-e29b-41d4-a716-446655440015",
        ),
    ];

    let response_texts = async {
        let mut response_texts = Vec::with_capacity(cases.len());
        for (body, delivery_id) in cases {
            let response = app
                .router
                .clone()
                .oneshot(webhook_request_for_event(
                    body,
                    SECRET,
                    delivery_id,
                    "merge_group",
                ))
                .await
                .expect("merge-group webhook request succeeds");
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
            response_texts.push(
                String::from_utf8(response_body(response).await)
                    .expect("merge-group response is UTF-8"),
            );
        }
        response_texts
    }
    .with_subscriber(subscriber)
    .await;

    let exposition = metrics(app.router).await;
    assert!(exposition.contains(
        "github_webhook_events_total{repository=\"owner/repository\",event_type=\"merge_group\",action=\"destroyed\"} 6"
    ));
    assert!(exposition
        .contains("github_merge_group_events_total{repository=\"owner/repository\",action=\"destroyed\",reason=\"other\"} 6"));
    let logs = captured_logs.text();
    assert!(!logs.contains("owner/repository"));
    assert!(response_texts
        .iter()
        .all(|response| !response.contains("owner/repository")));
    for output in response_texts
        .iter()
        .map(String::as_str)
        .chain([exposition.as_str(), logs.as_str()])
    {
        for forbidden in [
            "secret-group-sha",
            "merged\\nsha256=secret-group-sha",
            "550e8400-e29b-41d4-a716-446655440015",
            SECRET,
            malicious_signature.as_str(),
            std::str::from_utf8(MERGE_GROUP_DESTROYED_MALICIOUS)
                .expect("malicious payload is UTF-8"),
        ] {
            assert!(!output.contains(forbidden));
        }
    }
    let attempt_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM merge_queue_attempts")
        .fetch_one(&app.pool)
        .await
        .expect("merge-queue attempt count is readable");
    assert_eq!(attempt_count, 0);
}

#[tokio::test]
async fn duplicate_merge_group_delivery_does_not_repeat_event_metrics() {
    let app = test_app(2_097_152, Some(true)).await;

    for _ in 0..2 {
        let response = app
            .router
            .clone()
            .oneshot(webhook_request_for_event(
                MERGE_GROUP_DESTROYED_MERGED,
                SECRET,
                "550e8400-e29b-41d4-a716-446655440020",
                "merge_group",
            ))
            .await
            .expect("merge-group webhook request succeeds");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    let exposition = metrics(app.router).await;
    for expected in [
        "github_webhook_requests_total{repository=\"owner/repository\",result=\"accepted\"} 2",
        "github_webhook_duplicates_total{repository=\"owner/repository\"} 1",
        "github_webhook_events_total{repository=\"owner/repository\",event_type=\"merge_group\",action=\"destroyed\"} 1",
        "github_merge_group_events_total{repository=\"owner/repository\",action=\"destroyed\",reason=\"merged\"} 1",
    ] {
        assert!(exposition.contains(expected), "missing {expected:?}");
    }
}

#[tokio::test]
async fn unsupported_merge_group_action_updates_only_generic_metrics() {
    let app = test_app(2_097_152, Some(true)).await;

    let response = app
        .router
        .clone()
        .oneshot(webhook_request_for_event(
            MERGE_GROUP_UNSUPPORTED_ACTION,
            SECRET,
            "550e8400-e29b-41d4-a716-446655440030",
            "merge_group",
        ))
        .await
        .expect("merge-group webhook request succeeds");
    assert_eq!(response.status(), StatusCode::NO_CONTENT);

    let exposition = metrics(app.router).await;
    assert!(exposition
        .contains("github_webhook_events_total{repository=\"owner/repository\",event_type=\"merge_group\",action=\"created\"} 1"));
    for expected_zero in [
        "github_merge_group_events_total{repository=\"unknown\",action=\"checks_requested\",reason=\"none\"} 0",
        "github_merge_group_events_total{repository=\"unknown\",action=\"destroyed\",reason=\"merged\"} 0",
        "github_merge_group_events_total{repository=\"unknown\",action=\"destroyed\",reason=\"dequeued\"} 0",
        "github_merge_group_events_total{repository=\"unknown\",action=\"destroyed\",reason=\"invalidated\"} 0",
        "github_merge_group_events_total{repository=\"unknown\",action=\"destroyed\",reason=\"other\"} 0",
    ] {
        assert!(
            exposition.contains(expected_zero),
            "missing {expected_zero:?}"
        );
    }
}

#[tokio::test]
async fn authenticated_enabled_repository_returns_no_content() {
    let app = test_app(2_097_152, Some(true)).await;

    let response = app
        .router
        .oneshot(webhook_request(PAYLOAD, SECRET, DELIVERY_ID))
        .await
        .expect("webhook request succeeds");

    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response_body(response).await.is_empty());
}

#[tokio::test]
async fn authenticated_non_string_sha_fields_remain_accepted() {
    let app = test_app(2_097_152, Some(true)).await;
    let cases = [
        (
            PULL_REQUEST_NON_STRING_SHA,
            "pull_request",
            "550e8400-e29b-41d4-a716-446655440044",
        ),
        (
            MERGE_GROUP_NON_STRING_SHA,
            "merge_group",
            "550e8400-e29b-41d4-a716-446655440045",
        ),
    ];

    for (body, event_type, delivery_id) in cases {
        let response = app
            .router
            .clone()
            .oneshot(webhook_request_for_event(
                body,
                SECRET,
                delivery_id,
                event_type,
            ))
            .await
            .expect("webhook request succeeds");

        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(response_body(response).await.is_empty());
    }
}

#[tokio::test]
async fn malformed_unsupported_and_oversized_requests_have_stable_results() {
    let app = test_app(PAYLOAD.len() - 1, Some(true)).await;
    let missing_content_type = Request::builder()
        .method("POST")
        .uri("/webhooks/github")
        .body(Body::from(PAYLOAD))
        .expect("request is valid");
    let unsupported = Request::builder()
        .method("POST")
        .uri("/webhooks/github")
        .header(CONTENT_TYPE, "text/plain")
        .body(Body::from(PAYLOAD))
        .expect("request is valid");
    let malformed_signature = Request::builder()
        .method("POST")
        .uri("/webhooks/github")
        .header(CONTENT_TYPE, "application/json")
        .header("X-GitHub-Event", "push")
        .header("X-GitHub-Delivery", DELIVERY_ID)
        .header("X-Hub-Signature-256", "sha256=short")
        .body(Body::from(PAYLOAD))
        .expect("request is valid");
    let oversized = webhook_request(PAYLOAD, SECRET, DELIVERY_ID);

    let missing_response = app
        .router
        .clone()
        .oneshot(missing_content_type)
        .await
        .expect("request succeeds");
    let unsupported_response = app
        .router
        .clone()
        .oneshot(unsupported)
        .await
        .expect("request succeeds");
    let malformed_response = app
        .router
        .clone()
        .oneshot(malformed_signature)
        .await
        .expect("request succeeds");
    let oversized_response = app
        .router
        .clone()
        .oneshot(oversized)
        .await
        .expect("request succeeds");

    assert_eq!(missing_response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        unsupported_response.status(),
        StatusCode::UNSUPPORTED_MEDIA_TYPE
    );
    assert_eq!(malformed_response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(oversized_response.status(), StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        response_body(oversized_response).await,
        br#"{"code":"payload_too_large","message":"webhook payload is too large"}"#
    );
    let delivery_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM processed_deliveries")
        .fetch_one(&app.pool)
        .await
        .expect("delivery count is readable");
    assert_eq!(delivery_count, 0);
    let exposition = metrics(app.router).await;
    assert!(exposition
        .contains("github_webhook_requests_total{repository=\"unknown\",result=\"too_large\"} 1"));
    assert!(
        exposition.contains("github_webhook_request_body_bytes_count{repository=\"unknown\"} 0")
    );
    assert!(!exposition
        .contains("github_webhook_events_total{repository=\"owner/repository\",event_type=\"pull_request\",action=\"opened\"}"));
}

#[tokio::test]
async fn unauthorized_repository_outcomes_are_byte_identical() {
    let enabled = test_app(2_097_152, Some(true)).await;
    let disabled = test_app(2_097_152, Some(false)).await;
    let unknown = test_app(2_097_152, None).await;

    let wrong_signature = enabled
        .router
        .oneshot(webhook_request(PAYLOAD, "wrong-secret", DELIVERY_ID))
        .await
        .expect("request succeeds");
    let disabled_response = disabled
        .router
        .oneshot(webhook_request(PAYLOAD, SECRET, DELIVERY_ID))
        .await
        .expect("request succeeds");
    let unknown_response = unknown
        .router
        .oneshot(webhook_request(PAYLOAD, SECRET, DELIVERY_ID))
        .await
        .expect("request succeeds");

    let expected_body = br#"{"code":"unauthorized","message":"webhook authentication failed"}"#;
    let expected_headers = wrong_signature.headers().clone();
    for response in [wrong_signature, disabled_response, unknown_response] {
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers(), &expected_headers);
        assert_eq!(response_body(response).await, expected_body);
    }
}

#[tokio::test]
async fn malformed_json_delivery_and_repository_identity_return_bad_request() {
    let app = test_app(2_097_152, Some(true)).await;
    let malformed_json = br#"{"repository":"#;
    let invalid_repository = br#"{"repository":{"full_name":"invalid"}}"#;
    let requests = [
        webhook_request(malformed_json, SECRET, DELIVERY_ID),
        webhook_request(invalid_repository, SECRET, DELIVERY_ID),
        webhook_request(PAYLOAD, SECRET, "not-a-uuid"),
    ];

    for request in requests {
        let response = app
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("request succeeds");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            response_body(response).await,
            br#"{"code":"invalid_webhook","message":"webhook request is invalid"}"#
        );
    }

    let delivery_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM processed_deliveries")
        .fetch_one(&app.pool)
        .await
        .expect("delivery count is readable");
    assert_eq!(delivery_count, 0);
}

#[tokio::test]
async fn action_semantics_are_validated_only_after_authentication() {
    const NON_STRING_ACTION: &[u8] =
        br#"{"action":{"attacker":"value"},"repository":{"full_name":"owner/repository"}}"#;
    let unknown = test_app(2_097_152, None).await;
    let unknown_response = unknown
        .router
        .oneshot(webhook_request(NON_STRING_ACTION, SECRET, DELIVERY_ID))
        .await
        .expect("request succeeds");
    assert_eq!(unknown_response.status(), StatusCode::UNAUTHORIZED);

    let enabled = test_app(2_097_152, Some(true)).await;
    let enabled_response = enabled
        .router
        .oneshot(webhook_request(NON_STRING_ACTION, SECRET, DELIVERY_ID))
        .await
        .expect("request succeeds");
    assert_eq!(enabled_response.status(), StatusCode::BAD_REQUEST);
    let delivery_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM processed_deliveries")
        .fetch_one(&enabled.pool)
        .await
        .expect("delivery count is readable");
    assert_eq!(delivery_count, 0);
}

#[tokio::test]
async fn exact_body_and_signature_bytes_are_required_by_the_http_endpoint() {
    const CHANGED_PAYLOAD: &[u8] =
        br#"{"action":"closed","repository":{"full_name":"owner/repository"}}"#;
    let app = test_app(2_097_152, Some(true)).await;
    let original_signature = signature(SECRET, PAYLOAD);
    let response = app
        .router
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/github")
                .header(CONTENT_TYPE, "application/json")
                .header("X-GitHub-Event", "pull_request")
                .header("X-GitHub-Delivery", DELIVERY_ID)
                .header("X-Hub-Signature-256", original_signature)
                .body(Body::from(CHANGED_PAYLOAD))
                .expect("request is valid"),
        )
        .await
        .expect("request succeeds");

    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn restart_preserves_configuration_deduplication_and_bounded_metrics() {
    let directory = tempfile::tempdir().expect("temporary directory is created");
    let database_path = directory.path().join("restart.sqlite3");
    let first_pool = open_database(&database_path)
        .await
        .expect("first database instance opens");
    let cipher = RepositorySecretCipher::new(
        &MasterKey::from_slice(&[7_u8; 32]).expect("test master key is valid"),
    )
    .expect("repository cipher initializes");
    RepositoryStore::new(first_pool.clone(), cipher)
        .create(
            CanonicalRepositoryName::new("owner/repository").expect("repository name is valid"),
            RepositorySecret::new(SECRET.to_owned()).expect("secret is valid"),
            true,
        )
        .await
        .expect("repository configuration is created");
    first_pool.close().await;

    let second_pool = open_database(&database_path)
        .await
        .expect("second database instance opens");
    let cipher = RepositorySecretCipher::new(
        &MasterKey::from_slice(&[7_u8; 32]).expect("test master key is valid"),
    )
    .expect("repository cipher initializes");
    let admin_token = AdminToken::new("admin-token".to_owned()).expect("admin token is valid");
    let state = AppState::new(
        RepositoryStore::new(second_pool.clone(), cipher),
        AdminAuthenticator::new(&admin_token),
        2_097_152,
        DEFAULT_WORKFLOW_JOB_MAX_STEPS,
    );
    state
        .initialize_repository_metrics()
        .await
        .expect("repository metrics initialize after restart");
    let router = build_router(state);

    for _ in 0..2 {
        let response = router
            .clone()
            .oneshot(webhook_request(PAYLOAD, SECRET, DELIVERY_ID))
            .await
            .expect("webhook request succeeds after restart");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    let delivery_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM processed_deliveries")
        .fetch_one(&second_pool)
        .await
        .expect("delivery count is readable");
    assert_eq!(delivery_count, 1);
    let exposition = metrics(router).await;
    for expected in [
        "github_repository_configurations 1",
        "github_webhook_requests_total{repository=\"owner/repository\",result=\"accepted\"} 2",
        "github_webhook_events_total{repository=\"owner/repository\",event_type=\"pull_request\",action=\"opened\"} 1",
        "github_webhook_duplicates_total{repository=\"owner/repository\"} 1",
    ] {
        assert!(exposition.contains(expected), "missing {expected:?}");
    }
    for forbidden in [DELIVERY_ID, SECRET, "sha256="] {
        assert!(!exposition.contains(forbidden));
    }
    let persisted_payloads: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pragma_table_info('processed_deliveries') \
         WHERE name IN ('payload', 'repository_name', 'signature')",
    )
    .fetch_one(&second_pool)
    .await
    .expect("delivery schema is inspectable");
    assert_eq!(persisted_payloads, 0);
}

#[tokio::test]
async fn duplicate_delivery_updates_only_request_and_duplicate_metrics() {
    let app = test_app(2_097_152, Some(true)).await;

    for _ in 0..2 {
        let response = app
            .router
            .clone()
            .oneshot(webhook_request(PAYLOAD, SECRET, DELIVERY_ID))
            .await
            .expect("request succeeds");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
    }

    let delivery_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM processed_deliveries")
        .fetch_one(&app.pool)
        .await
        .expect("delivery count is readable");
    assert_eq!(delivery_count, 1);
    let exposition = metrics(app.router).await;
    assert!(exposition.contains(
        "github_webhook_requests_total{repository=\"owner/repository\",result=\"accepted\"} 2"
    ));
    assert!(exposition
        .contains("github_webhook_events_total{repository=\"owner/repository\",event_type=\"pull_request\",action=\"opened\"} 1"));
    assert!(exposition
        .contains("github_webhook_request_body_bytes_count{repository=\"owner/repository\"} 1"));
    assert!(
        exposition.contains("github_webhook_duplicates_total{repository=\"owner/repository\"} 1")
    );
}

#[tokio::test]
async fn normalized_logs_responses_and_metrics_exclude_sensitive_values() {
    let captured_logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(captured_logs.clone())
        .finish();
    let app = test_app(2_097_152, None).await;
    let supplied_signature = signature(SECRET, PAYLOAD);

    let response = app
        .router
        .clone()
        .oneshot(webhook_request(PAYLOAD, SECRET, DELIVERY_ID))
        .with_subscriber(subscriber)
        .await
        .expect("request succeeds");
    let response_text =
        String::from_utf8(response_body(response).await).expect("response body is UTF-8");
    let exposition = metrics(app.router).await;
    let logs = captured_logs.text();

    assert!(logs.contains("result=\"unauthorized\""));
    for output in [&response_text, &exposition, &logs] {
        for forbidden in [
            "owner/repository",
            DELIVERY_ID,
            SECRET,
            supplied_signature.as_str(),
            std::str::from_utf8(PAYLOAD).expect("payload is UTF-8"),
        ] {
            assert!(!output.contains(forbidden));
        }
    }
}

#[tokio::test]
async fn authentication_and_claim_database_failures_return_service_unavailable() {
    let authentication_app = test_app(2_097_152, Some(true)).await;
    authentication_app.pool.close().await;
    let authentication_logs = CapturedLogs::default();
    let authentication_subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(authentication_logs.clone())
        .finish();
    let authentication_response = authentication_app
        .router
        .clone()
        .oneshot(webhook_request(PAYLOAD, SECRET, DELIVERY_ID))
        .with_subscriber(authentication_subscriber)
        .await
        .expect("request succeeds");

    assert_eq!(
        authentication_response.status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let authentication_body: serde_json::Value =
        serde_json::from_slice(&response_body(authentication_response).await)
            .expect("authentication failure response is JSON");
    let authentication_error_id = authentication_body["error_id"]
        .as_str()
        .expect("authentication failure includes an error ID");
    uuid::Uuid::parse_str(authentication_error_id).expect("error ID is an opaque UUID");
    assert!(authentication_logs.text().contains(authentication_error_id));
    let authentication_exposition = metrics(authentication_app.router).await;
    assert!(authentication_exposition
        .contains("github_webhook_processing_failures_total{repository=\"unknown\",stage=\"authentication\"} 1"));
    assert!(authentication_exposition
        .contains("github_webhook_request_body_bytes_count{repository=\"unknown\"} 0"));

    let claim_app = test_app(2_097_152, Some(true)).await;
    sqlx::query("DROP TABLE processed_deliveries")
        .execute(&claim_app.pool)
        .await
        .expect("delivery table is removed");
    let claim_logs = CapturedLogs::default();
    let claim_subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(claim_logs.clone())
        .finish();
    let claim_response = claim_app
        .router
        .clone()
        .oneshot(webhook_request(PAYLOAD, SECRET, DELIVERY_ID))
        .with_subscriber(claim_subscriber)
        .await
        .expect("request succeeds");

    assert_eq!(claim_response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let claim_body: serde_json::Value =
        serde_json::from_slice(&response_body(claim_response).await)
            .expect("claim failure response is JSON");
    let claim_error_id = claim_body["error_id"]
        .as_str()
        .expect("claim failure includes an error ID");
    uuid::Uuid::parse_str(claim_error_id).expect("error ID is an opaque UUID");
    assert!(claim_logs.text().contains(claim_error_id));
    let exposition = metrics(claim_app.router).await;
    assert!(
        exposition.contains("github_webhook_processing_failures_total{repository=\"owner/repository\",stage=\"delivery_claim\"} 1")
    );
    assert!(
        exposition.contains("github_webhook_request_body_bytes_count{repository=\"unknown\"} 0")
    );
}
