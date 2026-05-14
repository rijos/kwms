//! Source abstraction — read raw raster windows + sample point values.
//!
//! See `docs/src/source-abstraction.md`. The trait exists as an internal seam,
//! not as a hedge for multiple backends; the only implementation is
//! [`gdal::GdalSource`].

pub mod gdal;
pub mod registry;

pub use self::registry::SourceRegistry;

use ::bytes::Bytes;

use crate::crs::{Bbox, Coord, Crs};

/// Per-source metadata, reported by `Source::metadata`.
#[derive(Debug, Clone)]
pub struct SourceMetadata {
    pub native_crs: Crs,
    pub native_bbox: Bbox,
    pub band_count: u32,
    pub pixel_type: PixelType,
    /// CRS-units per pixel at the source's native resolution.
    pub native_resolution: f64,
    pub nodata: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelType {
    U8,
    U16,
    I16,
    U32,
    I32,
    F32,
    F64,
}

/// Request a rectangular window of pixels. The source handles reprojection if
/// `crs` differs from its native CRS.
#[derive(Debug, Clone)]
pub struct WindowRequest {
    pub bbox: Bbox,
    pub crs: Crs,
    pub width: u32,
    pub height: u32,
    pub resampling: Resampling,
}

#[derive(Debug, Clone, Copy)]
pub enum Resampling {
    Nearest,
    Bilinear,
    Cubic,
    CubicSpline,
    Lanczos,
    Average,
    Mode,
}

/// A rendered pixel buffer plus shape/dtype/NoData metadata.
#[derive(Debug, Clone)]
pub struct PixelBuffer {
    pub width: u32,
    pub height: u32,
    pub bands: u32,
    pub pixel_type: PixelType,
    /// Row-major, band-interleaved.
    pub data: Bytes,
    pub nodata: Option<f64>,
}

/// One pixel value reported by `Source::sample`.
#[derive(Debug, Clone)]
pub struct PixelValue {
    pub band: u32,
    pub value: f64,
}

#[async_trait::async_trait]
pub trait Source: Send + Sync {
    fn metadata(&self) -> &SourceMetadata;
    async fn read_window(&self, req: WindowRequest) -> anyhow::Result<PixelBuffer>;
    async fn sample(&self, coord: Coord, crs: Crs) -> anyhow::Result<Vec<PixelValue>>;
}
