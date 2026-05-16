//! `kwms serve` — load config, build [`AppState`], and start the axum
//! server. Trace subscriber is initialised here (and only here) so
//! library-style users of the crate can install their own subscriber.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use tracing_subscriber::{fmt, EnvFilter};

use crate::server::AppState;

/// Run the server with the given config + secrets files. Returns only
/// when the listener exits (cleanly or with an error).
pub async fn run(config_path: PathBuf, secrets_path: PathBuf) -> Result<()> {
    init_tracing();

    let (config, secrets) =
        crate::config::load::load(&config_path, &secrets_path).with_context(|| {
            format!(
                "loading config (`{}`) and secrets (`{}`)",
                config_path.display(),
                secrets_path.display()
            )
        })?;

    let bind = config.server.bind.clone();
    tracing::info!(bind = %bind, "starting kwms server");

    let state = Arc::new(AppState::new(config, secrets).context("building application state")?);
    let app = crate::server::router(state);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding to `{bind}`"))?;
    axum::serve(listener, app)
        .await
        .context("axum serve loop exited with error")?;
    Ok(())
}

/// Install a JSON-friendly `tracing_subscriber` if one isn't already
/// installed. We `try_init` so re-entry (e.g. integration tests that boot
/// the server twice in one process) doesn't panic.
fn init_tracing() {
    let filter = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info,kwms=info"))
        .expect("static env filter parses");

    let _ = fmt()
        .with_env_filter(filter)
        .with_target(true)
        .json()
        .flatten_event(true)
        .try_init();
}
