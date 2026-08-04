use std::process::ExitCode;

use anyhow::{Context, Result};
use github_webhook_exporter::{
    app::{self, AppState},
    config::RuntimeConfig,
    security::{AdminAuthenticator, RepositorySecretCipher},
    storage::{self, RepositoryStore},
    telemetry,
};
use tokio::net::TcpListener;
use tracing::{error, info};

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
    );

    let bind_address = config.bind_address();
    let listener = TcpListener::bind(bind_address)
        .await
        .with_context(|| format!("failed to bind HTTP listener at {bind_address}"))?;
    info!(%bind_address, "HTTP server listening");

    app::serve(listener, state)
        .await
        .context("HTTP server failed")
}
