use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use axum::{
    body::{to_bytes, Body},
    http::{header, Method, Request, StatusCode},
    Router,
};
use github_webhook_exporter::{
    app::{build_router, AppState},
    config::DEFAULT_WORKFLOW_JOB_MAX_STEPS,
    security::{AdminAuthenticator, AdminToken, MasterKey, RepositorySecretCipher},
    storage::{open_database, RepositoryStore},
};
use serde_json::Value;
use sqlx::{Row, SqlitePool};
use tempfile::TempDir;
use tower::ServiceExt;
use tracing_subscriber::fmt::MakeWriter;

const ADMIN_TOKEN: &str = "independent-admin-token";
const MASTER_KEY_BYTES: &[u8; 32] = b"MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM";

struct TestApp {
    router: Router,
    pool: SqlitePool,
    _directory: TempDir,
}

fn router_for_pool(pool: SqlitePool) -> Router {
    let master_key = MasterKey::from_slice(MASTER_KEY_BYTES).expect("test key is valid");
    let cipher = RepositorySecretCipher::new(&master_key).expect("test cipher initializes");
    let admin_token = AdminToken::new(ADMIN_TOKEN.to_owned()).expect("test token is valid");
    build_router(AppState::new(
        RepositoryStore::new(pool, cipher),
        AdminAuthenticator::new(&admin_token),
        2_097_152,
        DEFAULT_WORKFLOW_JOB_MAX_STEPS,
    ))
}

impl TestApp {
    async fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary directory is created");
        let pool = open_database(&directory.path().join("api.db"))
            .await
            .expect("test database opens and migrates");
        Self {
            router: router_for_pool(pool.clone()),
            pool,
            _directory: directory,
        }
    }

    async fn request(
        &self,
        method: Method,
        uri: &str,
        authorization: Option<&str>,
        body: Body,
    ) -> axum::response::Response {
        let mut request = Request::builder().method(method).uri(uri);
        if let Some(value) = authorization {
            request = request.header(header::AUTHORIZATION, value);
        }

        self.router
            .clone()
            .oneshot(request.body(body).expect("request is valid"))
            .await
            .expect("router serves request")
    }

    async fn authorized_json(
        &self,
        method: Method,
        uri: &str,
        body: Value,
    ) -> axum::response::Response {
        let serialized = serde_json::to_vec(&body).expect("request body serializes");
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, "Bearer independent-admin-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serialized))
            .expect("request is valid");
        self.router
            .clone()
            .oneshot(request)
            .await
            .expect("router serves request")
    }

    async fn authorized_raw_json(
        &self,
        method: Method,
        uri: &str,
        body: &'static str,
    ) -> axum::response::Response {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::AUTHORIZATION, "Bearer independent-admin-token")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body))
            .expect("request is valid");
        self.router
            .clone()
            .oneshot(request)
            .await
            .expect("router serves request")
    }

    async fn create(&self, body: Value) -> axum::response::Response {
        self.authorized_json(Method::POST, "/api/v1/repositories", body)
            .await
    }
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

async fn response_body(response: axum::response::Response) -> Vec<u8> {
    to_bytes(response.into_body(), 70_000)
        .await
        .expect("response body is readable")
        .to_vec()
}

async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&response_body(response).await).expect("response body is JSON")
}

#[tokio::test]
async fn authentication_rejects_all_invalid_credentials_uniformly() {
    let app = TestApp::new().await;

    for authorization in [
        None,
        Some("Basic independent-admin-token"),
        Some("Bearer incorrect-admin-token"),
    ] {
        let response = app
            .request(
                Method::GET,
                "/api/v1/repositories",
                authorization,
                Body::empty(),
            )
            .await;

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(
            response.headers().get(header::WWW_AUTHENTICATE),
            Some(&header::HeaderValue::from_static("Bearer"))
        );
        assert_eq!(
            json_body(response).await,
            serde_json::json!({
                "code": "unauthorized",
                "message": "authentication required"
            })
        );
    }
}

#[tokio::test]
async fn repository_metadata_remains_available_after_process_state_restart() {
    let directory = tempfile::tempdir().expect("temporary directory is created");
    let database_path = directory.path().join("restart.db");
    let first_pool = open_database(&database_path)
        .await
        .expect("first database instance opens");
    let first_router = router_for_pool(first_pool.clone());
    let create = Request::builder()
        .method(Method::POST)
        .uri("/api/v1/repositories")
        .header(header::AUTHORIZATION, "Bearer independent-admin-token")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"full_name":"owner/persistent","webhook_secret":"restart-secret"}"#,
        ))
        .expect("request is valid");
    let created = first_router
        .oneshot(create)
        .await
        .expect("first router serves request");
    assert_eq!(created.status(), StatusCode::CREATED);
    first_pool.close().await;

    let second_pool = open_database(&database_path)
        .await
        .expect("second database instance opens");
    let list = Request::builder()
        .method(Method::GET)
        .uri("/api/v1/repositories")
        .header(header::AUTHORIZATION, "Bearer independent-admin-token")
        .body(Body::empty())
        .expect("request is valid");
    let listed = router_for_pool(second_pool)
        .oneshot(list)
        .await
        .expect("second router serves request");

    assert_eq!(listed.status(), StatusCode::OK);
    let body = String::from_utf8(response_body(listed).await).expect("response is UTF-8");
    assert!(body.contains("owner/persistent"));
    assert!(!body.contains("restart-secret"));
    assert!(!body.contains("webhook_secret"));
}

#[tokio::test]
async fn authentication_accepts_the_exact_admin_bearer_token() {
    let app = TestApp::new().await;

    let response = app
        .request(
            Method::GET,
            "/api/v1/repositories",
            Some("Bearer independent-admin-token"),
            Body::empty(),
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await, serde_json::json!([]));
}

#[tokio::test]
async fn create_defaults_enabled_and_returns_only_canonical_metadata() {
    let app = TestApp::new().await;

    let response = app
        .create(serde_json::json!({
            "full_name": " Owner/Repository ",
            "webhook_secret": "sensitive-webhook-secret"
        }))
        .await;

    assert_eq!(response.status(), StatusCode::CREATED);
    let body = json_body(response).await;
    assert_eq!(body["id"], 1);
    assert_eq!(body["full_name"], "owner/repository");
    assert_eq!(body["enabled"], true);
    assert!(body["created_at"].as_str().is_some());
    assert!(body["updated_at"].as_str().is_some());
    assert_eq!(
        body.as_object()
            .expect("metadata is an object")
            .keys()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>(),
        ["created_at", "enabled", "full_name", "id", "updated_at"]
            .into_iter()
            .map(str::to_owned)
            .collect()
    );
    assert!(!body.to_string().contains("sensitive-webhook-secret"));
}

#[tokio::test]
async fn create_accepts_explicit_disabled_and_list_is_ordered() {
    let app = TestApp::new().await;
    for (full_name, enabled) in [("owner/first", false), ("owner/second", true)] {
        let response = app
            .create(serde_json::json!({
                "full_name": full_name,
                "webhook_secret": format!("secret-for-{full_name}"),
                "enabled": enabled
            }))
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    let response = app
        .request(
            Method::GET,
            "/api/v1/repositories",
            Some("Bearer independent-admin-token"),
            Body::empty(),
        )
        .await;

    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body[0]["full_name"], "owner/first");
    assert_eq!(body[0]["enabled"], false);
    assert_eq!(body[1]["full_name"], "owner/second");
    assert_eq!(body[1]["enabled"], true);
}

#[tokio::test]
async fn create_rejects_conflicts_invalid_fields_and_secret_boundaries() {
    let app = TestApp::new().await;
    let first = app
        .create(serde_json::json!({
            "full_name": "OWNER/repository",
            "webhook_secret": "first-secret"
        }))
        .await;
    assert_eq!(first.status(), StatusCode::CREATED);

    let invalid_cases = [
        (
            serde_json::json!({
                "full_name": " owner/REPOSITORY ",
                "webhook_secret": "duplicate-secret"
            }),
            StatusCode::CONFLICT,
            "conflict",
        ),
        (
            serde_json::json!({"full_name": "invalid", "webhook_secret": "secret"}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            serde_json::json!({"full_name": "owner/new", "webhook_secret": ""}),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            serde_json::json!({
                "full_name": "owner/new",
                "webhook_secret": "x".repeat(65_537)
            }),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
        (
            serde_json::json!({
                "full_name": "owner/new",
                "webhook_secret": "secret",
                "unknown": true
            }),
            StatusCode::BAD_REQUEST,
            "invalid_request",
        ),
    ];

    for (request, expected_status, expected_code) in invalid_cases {
        let response = app.create(request).await;
        assert_eq!(response.status(), expected_status);
        assert_eq!(json_body(response).await["code"], expected_code);
    }
}

#[tokio::test]
async fn create_accepts_the_maximum_secret_length() {
    let app = TestApp::new().await;

    let response = app
        .create(serde_json::json!({
            "full_name": "owner/repository",
            "webhook_secret": "x".repeat(65_536)
        }))
        .await;

    assert_eq!(response.status(), StatusCode::CREATED);
}

#[tokio::test]
async fn create_maps_an_oversized_json_body_to_the_stable_invalid_request_error() {
    let app = TestApp::new().await;

    let response = app
        .create(serde_json::json!({
            "full_name": "owner/repository",
            "webhook_secret": "x".repeat(2 * 1024 * 1024)
        }))
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(
        json_body(response).await,
        serde_json::json!({
            "code": "invalid_request",
            "message": "request is invalid"
        })
    );
}

#[tokio::test]
async fn create_rejects_malformed_json_with_a_stable_error() {
    let app = TestApp::new().await;

    let response = app
        .authorized_raw_json(Method::POST, "/api/v1/repositories", "{malformed")
        .await;

    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(response).await["code"], "invalid_request");
}

#[tokio::test]
async fn get_returns_metadata_and_maps_unknown_or_invalid_ids() {
    let app = TestApp::new().await;
    let created = app
        .create(serde_json::json!({
            "full_name": "owner/repository",
            "webhook_secret": "secret"
        }))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);

    let response = app
        .request(
            Method::GET,
            "/api/v1/repositories/1",
            Some("Bearer independent-admin-token"),
            Body::empty(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["full_name"], "owner/repository");

    for (id, expected_status, expected_code) in [
        ("999", StatusCode::NOT_FOUND, "not_found"),
        ("0", StatusCode::BAD_REQUEST, "invalid_request"),
        ("-1", StatusCode::BAD_REQUEST, "invalid_request"),
        ("not-a-number", StatusCode::BAD_REQUEST, "invalid_request"),
    ] {
        let response = app
            .request(
                Method::GET,
                &format!("/api/v1/repositories/{id}"),
                Some("Bearer independent-admin-token"),
                Body::empty(),
            )
            .await;
        assert_eq!(response.status(), expected_status);
        assert_eq!(json_body(response).await["code"], expected_code);
    }
}

#[tokio::test]
async fn patch_accepts_name_secret_and_enabled_fields_independently() {
    let app = TestApp::new().await;
    let created = app
        .create(serde_json::json!({
            "full_name": "owner/repository",
            "webhook_secret": "original-secret"
        }))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);

    let renamed = app
        .authorized_json(
            Method::PATCH,
            "/api/v1/repositories/1",
            serde_json::json!({"full_name": "new-owner/new-repository"}),
        )
        .await;
    assert_eq!(renamed.status(), StatusCode::OK);
    assert_eq!(
        json_body(renamed).await["full_name"],
        "new-owner/new-repository"
    );

    let before_rotation: Vec<u8> =
        sqlx::query_scalar("SELECT webhook_secret_ciphertext FROM repositories WHERE id = 1")
            .fetch_one(&app.pool)
            .await
            .expect("stored ciphertext is readable");
    let rotated = app
        .authorized_json(
            Method::PATCH,
            "/api/v1/repositories/1",
            serde_json::json!({"webhook_secret": "rotated-secret"}),
        )
        .await;
    assert_eq!(rotated.status(), StatusCode::OK);
    let after_rotation: Vec<u8> =
        sqlx::query_scalar("SELECT webhook_secret_ciphertext FROM repositories WHERE id = 1")
            .fetch_one(&app.pool)
            .await
            .expect("rotated ciphertext is readable");
    assert_ne!(after_rotation, before_rotation);

    let disabled = app
        .authorized_json(
            Method::PATCH,
            "/api/v1/repositories/1",
            serde_json::json!({"enabled": false}),
        )
        .await;
    assert_eq!(disabled.status(), StatusCode::OK);
    assert_eq!(json_body(disabled).await["enabled"], false);
}

#[tokio::test]
async fn patch_updates_each_field_and_combines_rename_rotation_and_enablement() {
    let app = TestApp::new().await;
    let created = app
        .create(serde_json::json!({
            "full_name": "owner/repository",
            "webhook_secret": "original-secret"
        }))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let original = sqlx::query(
        "SELECT webhook_secret_ciphertext, webhook_secret_nonce FROM repositories WHERE id = 1",
    )
    .fetch_one(&app.pool)
    .await
    .expect("stored repository is readable");
    let original_ciphertext: Vec<u8> = original
        .try_get("webhook_secret_ciphertext")
        .expect("ciphertext has expected type");
    let original_nonce: Vec<u8> = original
        .try_get("webhook_secret_nonce")
        .expect("nonce has expected type");

    let enabled_response = app
        .authorized_json(
            Method::PATCH,
            "/api/v1/repositories/1",
            serde_json::json!({"enabled": false}),
        )
        .await;
    assert_eq!(enabled_response.status(), StatusCode::OK);
    assert_eq!(json_body(enabled_response).await["enabled"], false);

    let combined_response = app
        .authorized_json(
            Method::PATCH,
            "/api/v1/repositories/1",
            serde_json::json!({
                "full_name": " New-Owner/New.Repository ",
                "webhook_secret": "rotated-secret",
                "enabled": true
            }),
        )
        .await;
    assert_eq!(combined_response.status(), StatusCode::OK);
    let body = json_body(combined_response).await;
    assert_eq!(body["full_name"], "new-owner/new.repository");
    assert_eq!(body["enabled"], true);
    assert!(!body.to_string().contains("rotated-secret"));

    let rotated = sqlx::query(
        "SELECT webhook_secret_ciphertext, webhook_secret_nonce FROM repositories WHERE id = 1",
    )
    .fetch_one(&app.pool)
    .await
    .expect("updated repository is readable");
    assert_ne!(
        rotated
            .try_get::<Vec<u8>, _>("webhook_secret_ciphertext")
            .expect("ciphertext has expected type"),
        original_ciphertext
    );
    assert_ne!(
        rotated
            .try_get::<Vec<u8>, _>("webhook_secret_nonce")
            .expect("nonce has expected type"),
        original_nonce
    );
}

#[tokio::test]
async fn patch_rejects_empty_unknown_conflicting_missing_and_invalid_requests_atomically() {
    let app = TestApp::new().await;
    for (full_name, secret) in [
        ("owner/first", "first-secret"),
        ("owner/second", "second-secret"),
    ] {
        let response = app
            .create(serde_json::json!({
                "full_name": full_name,
                "webhook_secret": secret
            }))
            .await;
        assert_eq!(response.status(), StatusCode::CREATED);
    }

    for body in [
        serde_json::json!({}),
        serde_json::json!({"unknown": true}),
        serde_json::json!({"full_name": "invalid"}),
        serde_json::json!({"webhook_secret": ""}),
        serde_json::json!({"webhook_secret": "x".repeat(65_537)}),
    ] {
        let response = app
            .authorized_json(Method::PATCH, "/api/v1/repositories/2", body)
            .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(response).await["code"], "invalid_request");
    }

    let malformed = app
        .authorized_raw_json(Method::PATCH, "/api/v1/repositories/2", "{malformed")
        .await;
    assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json_body(malformed).await["code"], "invalid_request");

    let conflict = app
        .authorized_json(
            Method::PATCH,
            "/api/v1/repositories/2",
            serde_json::json!({
                "full_name": "OWNER/FIRST",
                "webhook_secret": "must-not-be-committed",
                "enabled": false
            }),
        )
        .await;
    assert_eq!(conflict.status(), StatusCode::CONFLICT);

    let unchanged = app
        .request(
            Method::GET,
            "/api/v1/repositories/2",
            Some("Bearer independent-admin-token"),
            Body::empty(),
        )
        .await;
    let unchanged = json_body(unchanged).await;
    assert_eq!(unchanged["full_name"], "owner/second");
    assert_eq!(unchanged["enabled"], true);

    for (id, expected_status, expected_code) in [
        ("999", StatusCode::NOT_FOUND, "not_found"),
        ("0", StatusCode::BAD_REQUEST, "invalid_request"),
        ("invalid", StatusCode::BAD_REQUEST, "invalid_request"),
    ] {
        let response = app
            .authorized_json(
                Method::PATCH,
                &format!("/api/v1/repositories/{id}"),
                serde_json::json!({"enabled": false}),
            )
            .await;
        assert_eq!(response.status(), expected_status);
        assert_eq!(json_body(response).await["code"], expected_code);
    }
}

#[tokio::test]
async fn delete_removes_repositories_and_maps_missing_or_invalid_ids() {
    let app = TestApp::new().await;
    let created = app
        .create(serde_json::json!({
            "full_name": "owner/repository",
            "webhook_secret": "secret"
        }))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);

    let response = app
        .request(
            Method::DELETE,
            "/api/v1/repositories/1",
            Some("Bearer independent-admin-token"),
            Body::empty(),
        )
        .await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert!(response_body(response).await.is_empty());

    for (id, expected_status, expected_code) in [
        ("1", StatusCode::NOT_FOUND, "not_found"),
        ("0", StatusCode::BAD_REQUEST, "invalid_request"),
        ("invalid", StatusCode::BAD_REQUEST, "invalid_request"),
    ] {
        let response = app
            .request(
                Method::DELETE,
                &format!("/api/v1/repositories/{id}"),
                Some("Bearer independent-admin-token"),
                Body::empty(),
            )
            .await;
        assert_eq!(response.status(), expected_status);
        assert_eq!(json_body(response).await["code"], expected_code);
    }
}

#[tokio::test]
async fn repository_configuration_gauge_initializes_from_durable_count() {
    let directory = tempfile::tempdir().expect("temporary directory is created");
    let pool = open_database(&directory.path().join("gauge.db"))
        .await
        .expect("test database opens and migrates");
    let master_key = MasterKey::from_slice(MASTER_KEY_BYTES).expect("test key is valid");
    let cipher = RepositorySecretCipher::new(&master_key).expect("test cipher initializes");
    let store = RepositoryStore::new(pool, cipher);
    for full_name in ["owner/first", "owner/second"] {
        store
            .create(
                github_webhook_exporter::security::CanonicalRepositoryName::new(full_name)
                    .expect("repository name is valid"),
                github_webhook_exporter::security::RepositorySecret::new(format!(
                    "secret-for-{full_name}"
                ))
                .expect("repository secret is valid"),
                true,
            )
            .await
            .expect("repository fixture is created");
    }
    let admin_token = AdminToken::new(ADMIN_TOKEN.to_owned()).expect("test token is valid");
    let state = AppState::new(
        store,
        AdminAuthenticator::new(&admin_token),
        2_097_152,
        DEFAULT_WORKFLOW_JOB_MAX_STEPS,
    );

    state
        .initialize_repository_metrics()
        .await
        .expect("repository metrics initialize");

    assert!(metrics_text(build_router(state))
        .await
        .contains("github_repository_configurations 2"));
}

#[tokio::test]
async fn repository_configuration_gauge_changes_only_after_successful_mutations() {
    let app = TestApp::new().await;

    let created = app
        .create(serde_json::json!({
            "full_name": "owner/repository",
            "webhook_secret": "repository-secret"
        }))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    assert!(metrics_text(app.router.clone())
        .await
        .contains("github_repository_configurations 1"));

    let conflict = app
        .create(serde_json::json!({
            "full_name": "OWNER/REPOSITORY",
            "webhook_secret": "conflicting-secret"
        }))
        .await;
    assert_eq!(conflict.status(), StatusCode::CONFLICT);
    assert!(metrics_text(app.router.clone())
        .await
        .contains("github_repository_configurations 1"));

    let deleted = app
        .request(
            Method::DELETE,
            "/api/v1/repositories/1",
            Some("Bearer independent-admin-token"),
            Body::empty(),
        )
        .await;
    assert_eq!(deleted.status(), StatusCode::NO_CONTENT);
    assert!(metrics_text(app.router.clone())
        .await
        .contains("github_repository_configurations 0"));

    let missing = app
        .request(
            Method::DELETE,
            "/api/v1/repositories/1",
            Some("Bearer independent-admin-token"),
            Body::empty(),
        )
        .await;
    assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    assert!(metrics_text(app.router)
        .await
        .contains("github_repository_configurations 0"));
}

async fn metrics_text(router: Router) -> String {
    let response = router
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .expect("metrics request is valid"),
        )
        .await
        .expect("metrics request succeeds");
    String::from_utf8(response_body(response).await).expect("metrics response is UTF-8")
}

#[tokio::test]
async fn authentication_protects_every_configuration_route() {
    let app = TestApp::new().await;
    let routes = [
        (Method::POST, "/api/v1/repositories"),
        (Method::GET, "/api/v1/repositories"),
        (Method::GET, "/api/v1/repositories/1"),
        (Method::PATCH, "/api/v1/repositories/1"),
        (Method::DELETE, "/api/v1/repositories/1"),
    ];

    for (method, uri) in routes {
        for authorization in [
            None,
            Some("Basic independent-admin-token"),
            Some("Bearer incorrect-admin-token"),
        ] {
            let response = app
                .request(method.clone(), uri, authorization, Body::from("{}"))
                .await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
            assert_eq!(
                response.headers().get(header::WWW_AUTHENTICATE),
                Some(&header::HeaderValue::from_static("Bearer"))
            );
            assert_eq!(json_body(response).await["code"], "unauthorized");
        }
    }
}

#[tokio::test]
async fn redaction_omits_security_material_from_responses_and_logs() {
    const REPOSITORY_NAME: &str = "sensitive-owner/sensitive-repository";
    const WEBHOOK_SECRET: &str = "plaintext-webhook-secret";
    let captured_logs = CapturedLogs::default();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(captured_logs.clone())
        .finish();
    let _subscriber_guard = tracing::subscriber::set_default(subscriber);
    let app = TestApp::new().await;

    let created = app
        .create(serde_json::json!({
            "full_name": REPOSITORY_NAME,
            "webhook_secret": WEBHOOK_SECRET
        }))
        .await;
    assert_eq!(created.status(), StatusCode::CREATED);
    let success_body =
        String::from_utf8(response_body(created).await).expect("success response body is UTF-8");

    sqlx::query("UPDATE repositories SET webhook_secret_ciphertext = ? WHERE id = 1")
        .bind(vec![0_u8; 4])
        .execute(&app.pool)
        .await
        .expect("test row is tampered");
    let failed = app
        .request(
            Method::GET,
            "/api/v1/repositories/1",
            Some("Bearer independent-admin-token"),
            Body::empty(),
        )
        .await;
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let error_body =
        String::from_utf8(response_body(failed).await).expect("error response body is UTF-8");
    let error_json: Value = serde_json::from_str(&error_body).expect("error response is JSON");
    assert_eq!(error_json["code"], "internal_error");
    assert_eq!(error_json["message"], "internal server error");
    let error_id = error_json["error_id"]
        .as_str()
        .expect("internal response includes an error ID");
    uuid::Uuid::parse_str(error_id).expect("error ID is an opaque UUID");

    for response in [&success_body, &error_body] {
        for forbidden in [
            WEBHOOK_SECRET,
            ADMIN_TOKEN,
            "MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM",
            "webhook_secret",
            "ciphertext",
            "nonce",
            "encryption_version",
            "admin_token",
            "master_key",
        ] {
            assert!(!response.contains(forbidden));
        }
    }

    let logs = captured_logs.text();
    for forbidden in [
        REPOSITORY_NAME,
        WEBHOOK_SECRET,
        ADMIN_TOKEN,
        "MMMMMMMMMMMMMMMMMMMMMMMMMMMMMMMM",
        "Authorization",
        "webhook_secret",
        "ciphertext",
        "nonce",
    ] {
        assert!(!logs.contains(forbidden));
    }
    assert!(logs.contains("outcome=\"internal_error\""));
    assert!(logs.contains(error_id));
}
