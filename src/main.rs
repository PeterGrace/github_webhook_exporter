use std::process::ExitCode;

use anyhow::{Context, Result};
use github_webhook_exporter::{
    app::{self, AppState, ShutdownOutcome},
    config::RuntimeConfig,
    lifecycle,
    retention::RetentionConfig,
    security::{AdminAuthenticator, RepositorySecretCipher},
    storage::{self, RepositoryStore},
    telemetry,
};
use tokio::net::TcpListener;
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Configuration can fail before the normal subscriber exists. This fallback preserves
            // structured error reporting without changing an already-installed subscriber.
            telemetry::init_fallback();
            error!(error = ?error, "application terminated");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<()> {
    let config = RuntimeConfig::from_env().context("failed to load runtime configuration")?;
    telemetry::init(config.rust_log()).context("failed to initialize local telemetry")?;

    let pool = storage::open_database(config.database_path())
        .await
        .context("failed to initialize SQLite storage")?;
    let cipher = RepositorySecretCipher::new(config.master_key())
        .context("failed to initialize repository-secret encryption")?;
    let state = AppState::new(
        RepositoryStore::new(pool, cipher),
        AdminAuthenticator::new(config.admin_token()),
        config.webhook_body_limit_bytes(),
    );
    state
        .initialize_repository_metrics()
        .await
        .context("failed to initialize repository configuration metrics")?;

    let configured_bind_address = config.bind_address();
    let shutdown_timeout = config.shutdown_timeout();
    let retention_config = RetentionConfig::new(
        config.delivery_prune_interval(),
        config.delivery_retention(),
    )
    .context("failed to initialize delivery retention")?;
    let listener = TcpListener::bind(configured_bind_address)
        .await
        .with_context(|| format!("failed to bind HTTP listener at {configured_bind_address}"))?;
    let bind_address = listener
        .local_addr()
        .context("failed to read bound HTTP listener address")?;
    info!(%bind_address, "HTTP server listening");

    let shutdown = async {
        match lifecycle::shutdown_signal().await {
            Ok(signal) => info!(?signal, "shutdown signal received"),
            Err(error) => error!(error = ?error, "failed to wait for shutdown signal"),
        }
    };
    let outcome = app::serve_with_shutdown(
        listener,
        state,
        shutdown,
        shutdown_timeout,
        retention_config,
    )
    .await
    .context("HTTP server failed")?;
    match outcome {
        ShutdownOutcome::Completed => info!("HTTP server stopped"),
        ShutdownOutcome::TimedOut => warn!(
            shutdown_timeout_seconds = shutdown_timeout.as_secs(),
            "HTTP server shutdown timed out"
        ),
    }

    Ok(())
}
