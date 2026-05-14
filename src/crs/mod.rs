//! CRS handling — opaque `Crs` tokens, value types, free functions backed by GDAL/PROJ.
//!
//! See `docs/src/crs-handling.md`.
//!
//! The free functions in this module are implemented in [`gdal`] (which is the
//! only module outside `source/gdal` and `encode` that calls into the `gdal`
//! crate). Callers should use these functions; they must not assume EPSG/PROJ
//! knowledge anywhere else.

mod gdal;

pub use self::gdal::{is_geographic, native_axis_order, transform_bbox, validate};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// An opaque CRS reference. Typically an `EPSG:NNNN` code; any authority
/// string GDAL/PROJ understands is accepted.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct Crs(pub String);

impl Crs {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Crs {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A 2D coordinate in some CRS's units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Coord {
    pub x: f64,
    pub y: f64,
}

/// An axis-aligned bounding box in some CRS's units.
///
/// Serializes as a 4-element array `[minx, miny, maxx, maxy]` to match the
/// `bbox: [..]` form used in `config.yaml`.
#[derive(Debug, Clone, Copy, PartialEq, JsonSchema)]
#[serde(from = "[f64; 4]", into = "[f64; 4]")]
#[derive(Serialize, Deserialize)]
pub struct Bbox {
    pub minx: f64,
    pub miny: f64,
    pub maxx: f64,
    pub maxy: f64,
}

impl From<[f64; 4]> for Bbox {
    fn from([minx, miny, maxx, maxy]: [f64; 4]) -> Self {
        Self { minx, miny, maxx, maxy }
    }
}

impl From<Bbox> for [f64; 4] {
    fn from(b: Bbox) -> Self {
        [b.minx, b.miny, b.maxx, b.maxy]
    }
}

/// CRS axis order — XY (east-north / lon-lat) or YX (north-east / lat-lon).
///
/// Affects WMS 1.1.1 vs 1.3.0 handling of geographic CRSes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisOrder {
    XY,
    YX,
}
