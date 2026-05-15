//! Open a `gdal::Dataset` with scoped credentials (per-source GDAL open options).
//!
//! Credentials are applied as **dataset-scoped open options** via
//! `gdal::DatasetOptions::open_options` — not as process-wide config — so
//! multiple sources can carry distinct credentials in the same process
//! (see `docs/src/source-abstraction.md`, "Credentials & VSI configuration").
//!
//! This is one of three modules permitted to call into the `gdal` crate
//! directly (alongside `crs/gdal.rs` and `encode.rs`).

use std::collections::HashMap;
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
    /// Open the dataset at `uri`, applying every (key, value) in `credentials`
    /// as a `KEY=VALUE` open option on this dataset only.
    pub fn open(uri: &str, credentials: &HashMap<String, String>) -> Result<Self> {
        // Build the `KEY=VALUE` strings. We must keep ownership of these
        // strings alive for the duration of the `open_ex` call, because
        // `DatasetOptions::open_options` borrows `&[&str]`.
        let kv_owned: Vec<String> = credentials
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect();
        let kv_refs: Vec<&str> = kv_owned.iter().map(String::as_str).collect();

        let options = DatasetOptions {
            open_flags: GdalOpenFlags::GDAL_OF_READONLY | GdalOpenFlags::GDAL_OF_RASTER,
            allowed_drivers: None,
            open_options: if kv_refs.is_empty() {
                None
            } else {
                Some(&kv_refs)
            },
            sibling_files: None,
        };

        // GDAL accepts both local filesystem paths and VSI prefixes
        // (`/vsiaz/`, `/vsis3/`, `/vsicurl/`, …) through the same path arg —
        // we pass the configured URI through verbatim.
        let dataset = Dataset::open_ex(Path::new(uri), options)
            .with_context(|| format!("failed to open GDAL dataset at '{uri}'"))?;

        Ok(Self {
            uri: uri.to_string(),
            dataset,
        })
    }
}
