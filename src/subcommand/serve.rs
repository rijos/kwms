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
    // Before anything can run a coordinate transform.
    crate::crs::disable_proj_network()?;

    let (config, secrets) =
        crate::config::load::load(&config_path, &secrets_path).with_context(|| {
            format!(
                "loading config (`{}`) and secrets (`{}`)",
                config_path.display(),
                secrets_path.display()
            )
        })?;

    let bind = config.server.bind.clone();
    let metrics_bind = config.server.metrics_bind.clone();
    tracing::info!(bind = %bind, metrics_bind = ?metrics_bind, "starting kwms server");

    let state = Arc::new(AppState::new(config, secrets).context("building application state")?);
    state.renderer.cache.check().await.context("cache is not usable")?;
    tracing::info!(tiers = ?state.renderer.cache.describe(), "cache ready");
    let app = crate::server::router(state);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding to `{bind}`"))?;
    let main = async {
        axum::serve(listener, app)
            .await
            .context("axum serve loop exited with error")
    };

    // `/metrics` lives on its own listener so it can be kept off the
    // public network (see `server.metrics_bind`).
    match metrics_bind {
        Some(addr) => {
            let metrics_listener = tokio::net::TcpListener::bind(&addr)
                .await
                .with_context(|| format!("binding metrics listener to `{addr}`"))?;
            let metrics = async {
                axum::serve(metrics_listener, crate::server::metrics_router())
                    .await
                    .context("metrics serve loop exited with error")
            };
            tokio::try_join!(main, metrics)?;
        }
        None => main.await?,
    }
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
