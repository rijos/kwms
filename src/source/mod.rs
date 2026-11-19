//! Source abstraction — read raw raster windows + sample point values.
//!
//! See `docs/src/source-abstraction.md`. The trait exists as an internal seam,
//! not as a hedge for multiple backends; the only implementation is
//! [`gdal::GdalSource`].

pub mod gdal;
pub mod registry;

pub use self::registry::SourceRegistry;

use ::bytes::Bytes;

use ::gdal::raster::ResampleAlg;

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
    /// Raster size in pixels at full resolution (width, height).
    pub size: (u32, u32),
    /// Band 1's overview sizes (width, height) — the overviews GDAL's
    /// `-ovr AUTO` chooses from when warping.
    pub overviews: Vec<(u32, u32)>,
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
    /// GDAL's own kernel enum — the layer's `resampling:` parsed by
    /// `ResampleAlg::from_str`, so there's no parallel name table to drift.
    pub resampling: ResampleAlg,
    /// Layer-level nodata override; `None` uses the source's own.
    pub nodata: Option<f64>,
}

/// A polygon area as WKB in a CRS: where a source has data (its tile
/// footprints or extent), or a layer's `source_mask`. Rasterized per
/// request by `source::gdal::mask`.
#[derive(Debug, Clone, PartialEq)]
pub struct Area {
    pub wkb: std::sync::Arc<Vec<u8>>,
    pub crs: Crs,
    /// Don't count the pixels its outline passes through. For a source's
    /// coverage: at coarse scales GTI snaps tile edges to its overview grid,
    /// so the outermost pixel row can mix imagery with the black "no tile"
    /// beyond — a thin line against no-data. Not for a `source_mask`
    /// (the data continues across its edges).
    pub trim_edges: bool,
}

#[derive(Debug, Clone)]
pub struct PixelBuffer {
    pub width: u32,
    pub height: u32,
    pub bands: u32,
    pub pixel_type: PixelType,
    /// Row-major, band-interleaved.
    pub data: Bytes,
    pub nodata: Option<f64>,
    /// Coverage, one byte per pixel (row-major): 255 = data, 0 = no data
    /// (outside the source or its `source_mask`, or nodata). `encode` turns
    /// it into an alpha band (PNG, WebP, GeoTIFF) or paints no-data pixels
    /// white (JPEG). `None`: every pixel is data.
    pub alpha: Option<Bytes>,
}

/// One pixel value reported by `Source::sample`.
/// Pixel element types that can be written as native-endian bytes — the
/// layout `PixelBuffer.data` uses. Safe replacement for reinterpreting
/// `Vec<T>` memory with `unsafe`.
pub(crate) trait NeBytes: Copy {
    fn extend_ne(self, out: &mut Vec<u8>);
}

macro_rules! impl_ne_bytes {
    ($($t:ty),*) => {$(
        impl NeBytes for $t {
            fn extend_ne(self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_ne_bytes());
            }
        }
    )*};
}
impl_ne_bytes!(u8, u16, i16, u32, i32, f32, f64);

/// Flatten typed samples into native-endian bytes.
pub(crate) fn to_ne_bytes<T: NeBytes>(values: Vec<T>) -> Vec<u8> {
    // Only a capacity hint; a real Vec's byte size always fits in usize.
    let mut out = Vec::with_capacity(values.len().checked_mul(std::mem::size_of::<T>()).unwrap_or(0));
    for v in values {
        v.extend_ne(&mut out);
    }
    out
}

impl PixelBuffer {
    /// A buffer where every sample is `fill` (cast to `pixel_type`, saturating).
    pub fn filled(
        width: u32,
        height: u32,
        bands: u32,
        pixel_type: PixelType,
        fill: f64,
        nodata: Option<f64>,
    ) -> anyhow::Result<Self> {
        let n = (width as usize)
            .checked_mul(height as usize)
            .and_then(|px| px.checked_mul(bands as usize))
            .ok_or_else(|| anyhow::anyhow!("PixelBuffer dimensions overflow usize"))?;
        let data = match pixel_type {
            PixelType::U8 => to_ne_bytes(vec![fill as u8; n]),
            PixelType::U16 => to_ne_bytes(vec![fill as u16; n]),
            PixelType::I16 => to_ne_bytes(vec![fill as i16; n]),
            PixelType::U32 => to_ne_bytes(vec![fill as u32; n]),
            PixelType::I32 => to_ne_bytes(vec![fill as i32; n]),
            PixelType::F32 => to_ne_bytes(vec![fill as f32; n]),
            PixelType::F64 => to_ne_bytes(vec![fill; n]),
        };
        Ok(Self {
            width,
            height,
            bands,
            pixel_type,
            data: Bytes::from(data),
            nodata,
            alpha: None,
        })
    }

    /// Whether every sample is the fill value — the nodata value, or 0 when
    /// there is none — i.e. the image holds no data. Floats compare by
    /// value (a NaN nodata matches any NaN, whatever its payload).
    pub fn is_all_fill(&self) -> bool {
        if let Some(alpha) = &self.alpha {
            return alpha.iter().all(|a| *a == 0);
        }
        let fill = self.nodata.unwrap_or(0.0);
        let floats = |size: usize, read: fn(&[u8]) -> Option<f64>| {
            let chunks = self.data.chunks_exact(size);
            chunks.remainder().is_empty()
                && chunks.into_iter().all(|c| {
                    read(c).is_some_and(|v| if fill.is_nan() { v.is_nan() } else { v == fill })
                })
        };
        match self.pixel_type {
            PixelType::F32 => floats(4, |c| <[u8; 4]>::try_from(c).ok().map(|b| f64::from(f32::from_ne_bytes(b)))),
            PixelType::F64 => floats(8, |c| <[u8; 8]>::try_from(c).ok().map(f64::from_ne_bytes)),
            _ => match Self::filled(1, 1, 1, self.pixel_type, fill, None) {
                Ok(one) => {
                    let chunks = self.data.chunks_exact(one.data.len());
                    chunks.remainder().is_empty() && chunks.into_iter().all(|c| c == one.data.as_ref())
                }
                Err(_) => false,
            },
        }
    }
}

#[derive(Debug, Clone)]
pub struct PixelValue {
    pub band: u32,
    pub value: f64,
}

#[async_trait::async_trait]
pub trait Source: Send + Sync {
    fn metadata(&self) -> &SourceMetadata;
    /// Where the source has data, in its native CRS: the footprints of a
    /// tile index's tiles, or a raster's extent. Pixels outside it render
    /// as no data. `None`: decided by nodata alone.
    fn coverage_area(&self) -> Option<Area> {
        None
    }
    async fn read_window(&self, req: WindowRequest) -> anyhow::Result<PixelBuffer>;
    async fn sample(&self, coord: Coord, crs: Crs) -> anyhow::Result<Vec<PixelValue>>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(pixel_type: PixelType, data: Vec<u8>, nodata: Option<f64>) -> PixelBuffer {
        PixelBuffer { width: 1, height: 1, bands: 1, pixel_type, data: Bytes::from(data), nodata, alpha: None }
    }

    #[test]
    fn is_all_fill_matches_nodata_or_zero() {
        // No nodata: all zeros is "no data"; any non-zero sample is data.
        assert!(PixelBuffer::filled(4, 4, 3, PixelType::U8, 0.0, None).unwrap().is_all_fill());
        assert!(!buffer(PixelType::U8, vec![0, 0, 1], None).is_all_fill());
        // Integer nodata, compared after the same cast `filled` uses.
        assert!(PixelBuffer::filled(2, 2, 1, PixelType::I16, -9999.0, Some(-9999.0)).unwrap().is_all_fill());
        assert!(!buffer(PixelType::I16, to_ne_bytes(vec![-9999i16, 0]), Some(-9999.0)).is_all_fill());
        // Float nodata by value; NaN nodata matches any NaN payload.
        assert!(buffer(PixelType::F32, to_ne_bytes(vec![-3.5f32; 3]), Some(-3.5)).is_all_fill());
        let odd_nan = f32::from_bits(0x7fc0_1234);
        assert!(buffer(PixelType::F32, to_ne_bytes(vec![f32::NAN, odd_nan]), Some(f64::NAN)).is_all_fill());
        assert!(!buffer(PixelType::F64, to_ne_bytes(vec![f64::NAN, 1.0]), Some(f64::NAN)).is_all_fill());
        assert!(!buffer(PixelType::F32, to_ne_bytes(vec![f32::NAN]), Some(0.0)).is_all_fill());
        // A truncated buffer is never "empty".
        assert!(!buffer(PixelType::U16, vec![0, 0, 0], None).is_all_fill());
    }
}
