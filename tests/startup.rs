use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use github_webhook_exporter::{
    security::{CanonicalRepositoryName, MasterKey, RepositorySecret, RepositorySecretCipher},
    storage::{open_database, RepositoryStore},
};

fn configured_command(database_path: &Path, bind_address: SocketAddr) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_github_webhook_exporter"));
    command
        .env_clear()
        .env("GHE_DATABASE_PATH", database_path)
        .env("GHE_MASTER_KEY", STANDARD.encode([7_u8; 32]))
        .env("GHE_ADMIN_TOKEN", "startup-admin-token-secret")
        .env("GHE_BIND_ADDRESS", bind_address.to_string())
        .env("GHE_SHUTDOWN_TIMEOUT_SECONDS", "2")
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", "http://127.0.0.1:0")
        .env("OTEL_EXPORTER_OTLP_TIMEOUT", "50")
        .env(
            "OTEL_EXPORTER_OTLP_HEADERS",
            "authorization=startup-otlp-header-secret",
        )
        .env("GHE_OTEL_SHUTDOWN_TIMEOUT_SECONDS", "1");
    command
}

fn unused_loopback_address() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("ephemeral listener binds");
    listener.local_addr().expect("listener has an address")
}

#[test]
fn invalid_configuration_reports_variable_without_leaking_credentials() {
    const INVALID_MASTER_KEY: &str = "invalid-master-key-secret";
    const ADMIN_TOKEN: &str = "invalid-admin-token-secret";
    let output = Command::new(env!("CARGO_BIN_EXE_github_webhook_exporter"))
        .env_clear()
        .env("GHE_DATABASE_PATH", "/tmp/exporter-invalid.db")
        .env("GHE_MASTER_KEY", INVALID_MASTER_KEY)
        .env("GHE_ADMIN_TOKEN", ADMIN_TOKEN)
        .output()
        .expect("exporter process starts");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("GHE_MASTER_KEY"));
    assert!(!stderr.contains(INVALID_MASTER_KEY));
    assert!(!stderr.contains(ADMIN_TOKEN));
}

#[test]
fn database_startup_failure_is_fatal_and_redacted() {
    let directory = tempfile::tempdir().expect("temporary directory is created");
    let database_path = directory.path().join("missing-parent/exporter.db");
    let output = configured_command(&database_path, unused_loopback_address())
        .output()
        .expect("exporter process starts");

    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains("failed to initialize SQLite storage"));
    assert!(stderr.contains("telemetry provider shutdown"));
    assert!(!stderr.contains("startup-admin-token-secret"));
    assert!(!stderr.contains("startup-otlp-header-secret"));
    assert!(!stderr.contains(&STANDARD.encode([7_u8; 32])));
    assert!(!stderr.contains(database_path.to_string_lossy().as_ref()));
}

#[cfg(unix)]
#[tokio::test]
async fn startup_metrics_are_initialized_before_readiness_is_served() {
    let directory = tempfile::tempdir().expect("temporary directory is created");
    let database_path = directory.path().join("exporter.db");
    let pool = open_database(&database_path)
        .await
        .expect("test database opens and migrates");
    let key = MasterKey::from_slice(&[7_u8; 32]).expect("test key is valid");
    let cipher = RepositorySecretCipher::new(&key).expect("test cipher initializes");
    RepositoryStore::new(pool.clone(), cipher)
        .create(
            CanonicalRepositoryName::new("owner/persisted").expect("repository name is valid"),
            RepositorySecret::new("persisted-secret".to_owned())
                .expect("repository secret is valid"),
            true,
        )
        .await
        .expect("repository fixture is created");
    pool.close().await;

    let bind_address = unused_loopback_address();
    let mut child = configured_command(&database_path, bind_address)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("exporter process starts");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut stream = loop {
        match TcpStream::connect(bind_address) {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                child.kill().expect("stalled exporter is killed");
                panic!("exporter did not bind before the test deadline: {error}");
            }
        }
    };
    stream
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("metrics request is written");
    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .expect("metrics response is read");

    child.kill().expect("exporter is stopped after assertion");
    let _output = child.wait_with_output().expect("exporter process exits");
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(response.contains("github_repository_configurations 1"));
    assert!(response.contains("github_workflow_job_steps"));
    assert!(response.contains("github_workflow_job_trace_rejections_total"));
}

#[cfg(unix)]
fn wait_with_deadline(mut child: Child, timeout: Duration) -> Output {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait().expect("child process status is readable") {
            Some(_status) => {
                return child
                    .wait_with_output()
                    .expect("completed exporter output is readable");
            }
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            None => {
                child.kill().expect("stalled exporter is killed");
                let output = child
                    .wait_with_output()
                    .expect("stalled exporter output is readable");
                panic!(
                    "exporter exceeded the combined shutdown deadline: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
            }
        }
    }
}

#[cfg(unix)]
fn assert_graceful_signal(signal: &str, expected_signal: &str) {
    let directory = tempfile::tempdir().expect("temporary directory is created");
    let bind_address = unused_loopback_address();
    let mut command = configured_command(&directory.path().join("exporter.db"), bind_address);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().expect("exporter process starts");
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut stream = loop {
        match TcpStream::connect(bind_address) {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(10)),
            Err(error) => {
                child.kill().expect("stalled exporter is killed");
                panic!("exporter did not bind before the test deadline: {error}");
            }
        }
    };
    stream
        .write_all(b"GET /health/ready HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("readiness request is written");
    let mut health_response = String::new();
    stream
        .read_to_string(&mut health_response)
        .expect("readiness response is read");
    assert!(health_response.starts_with("HTTP/1.1 200 OK\r\n"));

    let signal_status = Command::new("kill")
        .arg(signal)
        .arg(child.id().to_string())
        .status()
        .expect("kill command runs");
    assert!(signal_status.success());
    let output = wait_with_deadline(child, Duration::from_secs(5));

    assert!(output.status.success());
    let stderr = String::from_utf8(output.stderr).expect("stderr is UTF-8");
    assert!(stderr.contains(expected_signal));
    assert!(stderr.contains("HTTP server stopped"));
    assert!(stderr.contains("telemetry provider shutdown"));
    for signal in ["trace", "log"] {
        assert!(stderr.contains(&format!(
            "telemetry pipeline diagnostic kind=failure signal={signal} reason="
        )));
    }
    for captured in [&health_response, &stderr] {
        for forbidden in [
            "startup-admin-token-secret",
            "startup-otlp-header-secret",
            STANDARD.encode([7_u8; 32]).as_str(),
            "Authorization",
            "webhook_secret",
            "ciphertext",
            "nonce",
        ] {
            assert!(!captured.contains(forbidden));
        }
    }
}

#[cfg(unix)]
#[test]
fn sigterm_graceful_shutdown_and_health_redaction() {
    assert_graceful_signal("-TERM", "Terminate");
}

#[cfg(unix)]
#[test]
fn sigint_uses_the_same_graceful_shutdown_path() {
    assert_graceful_signal("-INT", "Interrupt");
}
