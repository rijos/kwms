//! Protocol request parsing, validation and response shaping. No I/O.
//!
//! Requests pass through two explicit stages before anything else sees them:
//! [`kvp::Kvp`] (strict query-string decoding, shared by WMS and WMTS) and
//! per-protocol parsing/validation into typed requests. Request errors are
//! [`error::OgcError`]s.

pub mod catalog;
pub mod error;
#[cfg(any(test, fuzzing))]
pub mod fuzz_harness;
pub mod kvp;
pub mod ows;

use anyhow::{Context, Result};

use crate::config::LayerConfig;
use crate::crs::{transform_bbox, Bbox, Crs};

/// A layer's configured `bbox` in `target`: exactly as configured when
/// `target` is its `bbox_crs`, else reprojected from it (GDAL, edges
/// densified — a superset). `None` without a configured bbox. Startup only
/// (capabilities documents are rendered once).
pub fn layer_bbox_in(layer: &LayerConfig, target: &Crs) -> Result<Option<Bbox>> {
    let Some(bbox) = layer.bbox else { return Ok(None) };
    let wgs84 = Crs::new("EPSG:4326");
    let from = layer.bbox_crs.as_ref().unwrap_or(&wgs84);
    let same = |a: &Crs, b: &Crs| a.as_str().eq_ignore_ascii_case(b.as_str());
    let lonlat = |c: &Crs| same(c, &wgs84) || c.as_str().eq_ignore_ascii_case("CRS:84");
    if same(from, target) || (lonlat(from) && lonlat(target)) {
        return Ok(Some(bbox));
    }
    transform_bbox(&bbox, from, target)
        .map(Some)
        .with_context(|| format!("bbox of layer `{}` from {from} to {target}", layer.name))
}
pub mod query;
pub mod tms;
pub mod wms;
pub mod wmts;
