//! `kwms serve` — load config, build [`AppState`], and start the axum
//! server. Trace subscriber is initialised here (and only here) so
//! library-style users of the crate can install their own subscriber.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::server::AppState;

/// Run the server with the given config + secrets files. Returns only
/// when the listener exits (cleanly or with an error).
pub async fn run(config_path: PathBuf, secrets_path: PathBuf, logging: &crate::logging::Logging) -> Result<()> {
    // Before anything can run a coordinate transform.
    crate::crs::disable_proj_network()?;

    let (config, secrets) = crate::config::load::load(&config_path, &secrets_path).with_context(|| {
        format!(
            "loading config (`{}`) and secrets (`{}`)",
            config_path.display(),
            secrets_path.display()
        )
    })?;
    logging.apply(&config.server.log_level)?;
    // Before any source is opened.
    crate::source::gdal::runtime::configure(&crate::source::gdal::runtime::GdalRuntime::serving(&config.server))?;

    let bind = config.server.bind.clone();
    let metrics_bind = config.server.metrics_bind.clone();
    tracing::info!(bind = %bind, metrics_bind = ?metrics_bind, "starting kwms server");

    let state = Arc::new(AppState::new(config, secrets).context("building application state")?);
    state.renderer.cache.check().await.context("cache is not usable")?;
    tracing::info!(tiers = ?state.renderer.cache.describe(), "cache ready");
    let limits = Limits {
        header_read_timeout: state.config.server.header_read_timeout,
        max_connections: state.config.server.max_connections,
    };
    let app = crate::server::router(state);

    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .with_context(|| format!("binding to `{bind}`"))?;
    let main = async {
        serve_http1(listener, app, limits)
            .await
            .context("HTTP serve loop exited with error")
    };

    // `/metrics` lives on its own listener so it can be kept off the
    // public network (see `server.metrics_bind`).
    match metrics_bind {
        Some(addr) => {
            let metrics_listener = tokio::net::TcpListener::bind(&addr)
                .await
                .with_context(|| format!("binding metrics listener to `{addr}`"))?;
            let metrics_limits = Limits {
                max_connections: 64,
                ..limits
            };
            let metrics = async {
                serve_http1(metrics_listener, crate::server::metrics_router(), metrics_limits)
                    .await
                    .context("metrics serve loop exited with error")
            };
            tokio::try_join!(main, metrics)?;
        }
        None => main.await?,
    }
    Ok(())
}

/// Per-listener connection limits.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Time to receive a request's headers, which is also the longest an
    /// idle keep-alive connection stays open.
    pub header_read_timeout: std::time::Duration,
    /// Connections open at once; more wait in the listen backlog.
    pub max_connections: usize,
}

/// Serve `app` on `listener`: HTTP/1.1 only, with the header-read timeout
/// enforced (it needs a timer, which `axum::serve` doesn't give hyper, so
/// there it was silently off) and at most `max_connections` connections.
/// kwms sits behind a TLS-terminating proxy; HTTP/2 (and cleartext h2c) is
/// never spoken.
pub async fn serve_http1(listener: tokio::net::TcpListener, app: axum::Router, limits: Limits) -> Result<()> {
    use hyper::server::conn::http1;
    use hyper_util::rt::{TokioIo, TokioTimer};
    use hyper_util::service::TowerToHyperService;

    let slots = Arc::new(tokio::sync::Semaphore::new(limits.max_connections));
    loop {
        // Wait for a free slot before accepting: excess connections queue in
        // the kernel's backlog instead of costing a task each.
        let permit = slots
            .clone()
            .acquire_owned()
            .await
            .context("connection limiter closed")?;
        let (stream, _peer) = match listener.accept().await {
            Ok(conn) => conn,
            Err(e) => {
                // E.g. out of file descriptors: back off instead of spinning.
                tracing::warn!(error = %e, "accepting a connection failed");
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let _ = stream.set_nodelay(true);
        let service = TowerToHyperService::new(app.clone());
        tokio::spawn(async move {
            let _permit = permit;
            let connection = http1::Builder::new()
                .timer(TokioTimer::new())
                .header_read_timeout(limits.header_read_timeout)
                .keep_alive(true)
                .serve_connection(TokioIo::new(stream), service);
            if let Err(e) = connection.await {
                tracing::debug!(error = %e, "connection ended with an error");
            }
        });
    }
}
