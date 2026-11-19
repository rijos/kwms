//! Open a `gdal::Dataset` read-only with the source's GDAL open options.
//!
//! Credentials are *not* open options: GDAL's VSI handlers read them from
//! path-specific options, registered once at startup by
//! [`super::credentials::register`].
//!
//! This is one of three modules permitted to call into the `gdal` crate
//! directly (alongside `crs/gdal.rs` and `encode.rs`).

use std::path::Path;

use anyhow::{Context, Result};
use gdal::{Dataset, DatasetOptions, GdalOpenFlags};

/// A thin, typed wrapper around an open `gdal::Dataset` plus the URI it came
/// from (kept for diagnostics).
pub struct OpenedDataset {
    pub uri: String,
    pub dataset: Dataset,
}

impl OpenedDataset {
    /// Open the dataset at `uri` with `open_options` (`KEY=VALUE` strings,
    /// as `gdalinfo -oo` takes them).
    pub fn open(uri: &str, open_options: &[String]) -> Result<Self> {
        let refs: Vec<&str> = open_options.iter().map(String::as_str).collect();
        let options = DatasetOptions {
            open_flags: GdalOpenFlags::GDAL_OF_READONLY | GdalOpenFlags::GDAL_OF_RASTER,
            allowed_drivers: None,
            open_options: if refs.is_empty() { None } else { Some(&refs) },
            sibling_files: None,
        };

        // GDAL accepts local paths, VSI prefixes (`/vsiaz/`, `/vsis3/`, …) and
        // driver prefixes (`GTI:`) through the same argument — the configured
        // URI is passed through verbatim.
        let dataset = Dataset::open_ex(Path::new(uri), options)
            .with_context(|| format!("failed to open GDAL dataset at '{uri}'"))?;

        Ok(Self {
            uri: uri.to_string(),
            dataset,
        })
    }
}
