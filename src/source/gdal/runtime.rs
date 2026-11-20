//! Process-wide GDAL settings, applied once at startup before any dataset
//! is opened: what GDAL may read from object stores, how long a remote read
//! may take, how many threads a tile index may use, and where GDAL's
//! messages go.
//!
//! * **Only `.tif`/`.tiff` from remote.** `CPL_VSIL_CURL_ALLOWED_EXTENSIONS`
//!   makes every network file system (`/vsiaz/`, `/vsis3/`, `/vsicurl/`, …)
//!   treat any other name as nonexistent without asking the server — no
//!   `.aux.xml`/`.ovr` sidecars, no `.vrt` documents. Directory listings are
//!   off too (`GDAL_DISABLE_READDIR_ON_OPEN`): opening a COG costs one
//!   request less, and local sidecars are still found by name.
//! * **Timeouts while serving.** A stalled object-store connection would
//!   otherwise hold a dataset-pool worker forever (GDAL's default is no
//!   timeout); after a few, a layer answers only 429. `kwms cache update`
//!   runs without them: rendering a country-wide coarse tile legitimately
//!   takes long.
//! * **Tile-index threads.** The GTI driver reads the tiles under one
//!   request in parallel on GDAL's own thread pool, by default with one
//!   thread per CPU. Serving caps that (tiles past the cached levels touch
//!   at most a few files); `cache update` keeps all CPUs.
//! * **Messages.** GDAL prints errors and warnings to stderr; they go to
//!   `tracing` instead (target `gdal`), with credentials removed.

use std::sync::Once;
use std::time::Duration;

use anyhow::{Context, Result};

/// Remote file names GDAL may open: the sources' COGs.
pub const REMOTE_EXTENSIONS: &str = ".tif,.tiff";

/// How kwms runs GDAL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GdalRuntime {
    /// Limit on one HTTP request to an object store; `None`: no limit.
    pub http_timeout: Option<Duration>,
    /// Threads one tile-index read may use; `None`: GDAL's default (all CPUs).
    pub tile_index_threads: Option<usize>,
}

impl GdalRuntime {
    /// `kwms serve`.
    pub fn serving(server: &crate::config::ServerConfig) -> Self {
        Self {
            http_timeout: Some(server.source_timeout),
            tile_index_threads: Some(server.tile_index_threads),
        }
    }

    /// `kwms cache update`: no timeouts, all CPUs.
    pub fn batch() -> Self {
        Self {
            http_timeout: None,
            tile_index_threads: None,
        }
    }

    /// The GDAL configuration options this runtime sets.
    pub fn options(&self) -> Vec<(&'static str, String)> {
        let mut out = vec![
            ("CPL_VSIL_CURL_ALLOWED_EXTENSIONS", REMOTE_EXTENSIONS.to_string()),
            ("GDAL_DISABLE_READDIR_ON_OPEN", "TRUE".to_string()),
        ];
        if let Some(t) = self.http_timeout {
            let secs = t.as_secs().max(1);
            out.push(("GDAL_HTTP_TIMEOUT", secs.to_string()));
            out.push(("GDAL_HTTP_CONNECTTIMEOUT", secs.min(5).to_string()));
            // A connection that stops delivering data fails well before the
            // overall timeout.
            out.push(("GDAL_HTTP_LOW_SPEED_TIME", secs.min(10).to_string()));
            out.push(("GDAL_HTTP_LOW_SPEED_LIMIT", "1024".to_string()));
        }
        if let Some(n) = self.tile_index_threads {
            out.push(("GTI_NUM_THREADS", n.max(1).to_string()));
        }
        out
    }
}

/// Apply `runtime` (see the module docs). Call once, at startup, before any
/// dataset is opened: GDAL reads configuration options without a lock.
pub fn configure(runtime: &GdalRuntime) -> Result<()> {
    static HANDLER: Once = Once::new();
    HANDLER.call_once(crate::ffi::set_error_handler);
    for (key, value) in runtime.options() {
        gdal::config::set_config_option(key, &value).with_context(|| format!("setting GDAL option {key}"))?;
    }
    tracing::info!(options = ?runtime.options(), "GDAL configured");
    Ok(())
}

/// Log one GDAL message (called from [`crate::ffi`]'s error handler).
pub(crate) fn log_gdal_message(class: gdal_sys::CPLErr::Type, number: i32, message: &str) {
    use gdal_sys::CPLErr;
    let message = crate::redact::secrets(message);
    // kwms passes GTI options the driver handles but doesn't declare
    // (OVERVIEW_<n>_*), so GDAL warns about them on every open.
    if message.contains("does not support open option") {
        tracing::debug!(target: "gdal", code = number, "{message}");
        return;
    }
    match class {
        CPLErr::CE_Fatal => tracing::error!(target: "gdal", code = number, "{message}"),
        CPLErr::CE_Failure | CPLErr::CE_Warning => tracing::warn!(target: "gdal", code = number, "{message}"),
        _ => tracing::debug!(target: "gdal", code = number, "{message}"),
    }
}

/// Whether `location` (a tile of a tile index) names a GeoTIFF.
pub fn is_tiff(location: &str) -> bool {
    let lower = location.to_ascii_lowercase();
    lower.ends_with(".tif") || lower.ends_with(".tiff")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serving_sets_timeouts_and_threads_batch_does_not() {
        let server = crate::config::ServerConfig::default();
        let serving = GdalRuntime::serving(&server).options();
        let keys: Vec<_> = serving.iter().map(|(k, _)| *k).collect();
        assert!(keys.contains(&"GDAL_HTTP_TIMEOUT"));
        assert!(keys.contains(&"GTI_NUM_THREADS"));
        assert!(keys.contains(&"CPL_VSIL_CURL_ALLOWED_EXTENSIONS"));

        let batch = GdalRuntime::batch().options();
        let keys: Vec<_> = batch.iter().map(|(k, _)| *k).collect();
        assert!(!keys.contains(&"GDAL_HTTP_TIMEOUT"));
        assert!(!keys.contains(&"GTI_NUM_THREADS"));
        assert!(keys.contains(&"CPL_VSIL_CURL_ALLOWED_EXTENSIONS"));
    }

    #[test]
    fn tiff_names() {
        assert!(is_tiff("/vsiaz/c/a/100000_370000.tif"));
        assert!(is_tiff("x.TIFF"));
        assert!(!is_tiff("/vsiaz/c/a/x.vrt"));
        assert!(!is_tiff("/vsiaz/c/a/x.tif.aux.xml"));
    }
}
