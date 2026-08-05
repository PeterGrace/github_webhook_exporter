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
    security::{
        AdminAuthenticator, AdminToken, CanonicalRepositoryName, MasterKey, RepositorySecret,
        RepositorySecretCipher,
    },
    storage::{open_database, RepositoryStore},
};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::SqlitePool;
use tower::ServiceExt;
use tracing::instrument::WithSubscriber;
use tracing_subscriber::fmt::MakeWriter;

const SECRET: &str = "webhook-test-secret";
const DELIVERY_ID: &str = "550e8400-e29b-41d4-a716-446655440000";
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

async fn test_app(body_limit: usize, enabled: Option<bool>) -> TestApp {
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
    if let Some(enabled) = enabled {
        store
            .create(
                CanonicalRepositoryName::new("owner/repository").expect("repository name is valid"),
                RepositorySecret::new(SECRET.to_owned()).expect("secret is valid"),
                enabled,
            )
            .await
            .expect("repository fixture is created");
    }
    let admin_token = AdminToken::new("admin-token".to_owned()).expect("admin token is valid");
    let state = AppState::new(store, AdminAuthenticator::new(&admin_token), body_limit);

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

fn webhook_request(
    body: &'static [u8],
    repository_secret: &str,
    delivery_id: &str,
) -> Request<Body> {
    webhook_request_for_event(body, repository_secret, delivery_id, "pull_request")
}

fn webhook_request_for_event(
    body: &'static [u8],
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
        .body(Body::from(body))
        .expect("webhook request is valid")
}

async fn response_body(response: Response) -> Vec<u8> {
    to_bytes(response.into_body(), 16 * 1_024)
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
        "github_webhook_events_total{event_type=\"merge_group\",action=\"checks_requested\"} 1",
        "github_webhook_events_total{event_type=\"merge_group\",action=\"destroyed\"} 4",
        "github_merge_group_events_total{action=\"checks_requested\",reason=\"none\"} 1",
        "github_merge_group_events_total{action=\"destroyed\",reason=\"merged\"} 1",
        "github_merge_group_events_total{action=\"destroyed\",reason=\"dequeued\"} 1",
        "github_merge_group_events_total{action=\"destroyed\",reason=\"invalidated\"} 1",
        "github_merge_group_events_total{action=\"destroyed\",reason=\"other\"} 1",
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
        "github_webhook_events_total{event_type=\"merge_group\",action=\"destroyed\"} 6"
    ));
    assert!(exposition
        .contains("github_merge_group_events_total{action=\"destroyed\",reason=\"other\"} 6"));
    let logs = captured_logs.text();
    for output in response_texts
        .iter()
        .map(String::as_str)
        .chain([exposition.as_str(), logs.as_str()])
    {
        for forbidden in [
            "owner/repository",
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
        "github_webhook_requests_total{result=\"accepted\"} 2",
        "github_webhook_duplicates_total 1",
        "github_webhook_events_total{event_type=\"merge_group\",action=\"destroyed\"} 1",
        "github_merge_group_events_total{action=\"destroyed\",reason=\"merged\"} 1",
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
        .contains("github_webhook_events_total{event_type=\"merge_group\",action=\"created\"} 1"));
    for expected_zero in [
        "github_merge_group_events_total{action=\"checks_requested\",reason=\"none\"} 0",
        "github_merge_group_events_total{action=\"destroyed\",reason=\"merged\"} 0",
        "github_merge_group_events_total{action=\"destroyed\",reason=\"dequeued\"} 0",
        "github_merge_group_events_total{action=\"destroyed\",reason=\"invalidated\"} 0",
        "github_merge_group_events_total{action=\"destroyed\",reason=\"other\"} 0",
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
    assert!(exposition.contains("github_webhook_requests_total{result=\"too_large\"} 1"));
    assert!(exposition.contains("github_webhook_request_body_bytes_count 0"));
    assert!(!exposition
        .contains("github_webhook_events_total{event_type=\"pull_request\",action=\"opened\"}"));
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
        "github_webhook_requests_total{result=\"accepted\"} 2",
        "github_webhook_events_total{event_type=\"pull_request\",action=\"opened\"} 1",
        "github_webhook_duplicates_total 1",
    ] {
        assert!(exposition.contains(expected), "missing {expected:?}");
    }
    for forbidden in ["owner/repository", DELIVERY_ID, SECRET, "sha256="] {
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
    assert!(exposition.contains("github_webhook_requests_total{result=\"accepted\"} 2"));
    assert!(exposition
        .contains("github_webhook_events_total{event_type=\"pull_request\",action=\"opened\"} 1"));
    assert!(exposition.contains("github_webhook_request_body_bytes_count 1"));
    assert!(exposition.contains("github_webhook_duplicates_total 1"));
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
        .contains("github_webhook_processing_failures_total{stage=\"authentication\"} 1"));
    assert!(authentication_exposition.contains("github_webhook_request_body_bytes_count 0"));

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
        exposition.contains("github_webhook_processing_failures_total{stage=\"delivery_claim\"} 1")
    );
    assert!(exposition.contains("github_webhook_request_body_bytes_count 0"));
}
