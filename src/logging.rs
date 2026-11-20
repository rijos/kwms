//! Logging: `tracing` events as JSON lines on the console, one per event,
//! for the platform (container runtime, log shipper) to collect. Nothing in
//! kwms prints log output any other way; GDAL's messages are routed here
//! too (see [`crate::source::gdal::runtime`]).
//!
//! The filter comes from `RUST_LOG` if set, else from `server.log_level` in
//! the config, applied once the config is loaded (until then: `info`).

use anyhow::{anyhow, bail, Result};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, reload, EnvFilter, Registry};

/// Where log lines go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Console {
    /// Long-running commands (`serve`, `cache update`).
    Stdout,
    /// Commands whose stdout is their result (`config check`, `config schema`).
    Stderr,
}

/// The installed subscriber's filter, adjustable once the config is known.
pub struct Logging {
    handle: Option<reload::Handle<EnvFilter, Registry>>,
    /// `RUST_LOG` was set: it wins over the config.
    from_env: bool,
}

/// Install the JSON subscriber (once per process; later calls only return a
/// handle that does nothing, e.g. in tests).
pub fn init(console: Console) -> Logging {
    let env = std::env::var("RUST_LOG").ok().filter(|v| !v.trim().is_empty());
    let from_env = env.is_some();
    let filter = env
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));
    let (filter, handle) = reload::Layer::new(filter);
    let json = fmt::layer().json().flatten_event(true).with_target(true);
    let installed = match console {
        Console::Stdout => tracing_subscriber::registry()
            .with(filter)
            .with(json.with_writer(std::io::stdout))
            .try_init(),
        Console::Stderr => tracing_subscriber::registry()
            .with(filter)
            .with(json.with_writer(std::io::stderr))
            .try_init(),
    };
    Logging {
        handle: installed.is_ok().then_some(handle),
        from_env,
    }
}

impl Logging {
    /// Apply the config's `server.log_level` (ignored when `RUST_LOG` is set).
    pub fn apply(&self, log_level: &str) -> Result<()> {
        if self.from_env {
            return Ok(());
        }
        let Some(handle) = &self.handle else { return Ok(()) };
        handle
            .reload(parse_filter(log_level)?)
            .map_err(|e| anyhow!("applying log_level: {e}"))
    }
}

const LEVELS: &[&str] = &["off", "error", "warn", "info", "debug", "trace"];

/// `log_level`: a level (`info`) or `RUST_LOG`-style directives
/// (`info,kwms::server=debug,gdal=warn`), each with a known level.
pub fn parse_filter(log_level: &str) -> Result<EnvFilter> {
    let directives: Vec<&str> = log_level.split(',').map(str::trim).collect();
    if directives.iter().any(|d| d.is_empty()) {
        bail!("log_level '{log_level}' has an empty directive");
    }
    for d in &directives {
        let level = d.rsplit_once('=').map_or(*d, |(_, l)| l);
        if !LEVELS.contains(&level.to_ascii_lowercase().as_str()) {
            bail!("log_level '{log_level}': '{d}' needs a level, one of {LEVELS:?}");
        }
    }
    EnvFilter::try_new(directives.join(",")).map_err(|e| anyhow!("log_level '{log_level}': {e}"))
}

/// Log a command's fatal error (credentials removed) — the last line a
/// failing command writes.
pub fn fatal(err: &anyhow::Error) {
    let chain = format!("{err:#}");
    tracing::error!(error = %crate::redact::secrets(&chain), "kwms failed");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_levels_parse() {
        for ok in ["info", "DEBUG", "warn,kwms::server=debug", "info, gdal=error"] {
            assert!(parse_filter(ok).is_ok(), "{ok}");
        }
        for bad in ["", "infoo", "info,", "kwms=loud"] {
            assert!(parse_filter(bad).is_err(), "{bad}");
        }
    }
}
