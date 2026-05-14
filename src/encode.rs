//! Output encoding — GDAL-backed for PNG / JPEG / WebP / GeoTIFF.
//!
//! See `docs/src/output-encoding.md`. One of three modules permitted to call
//! into the `gdal` crate directly (alongside `crs/gdal.rs` and `source/gdal/`).

use anyhow::Result;
use bytes::Bytes;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::crs::{Bbox, Crs};
use crate::source::PixelBuffer;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    Png,
    Jpeg,
    Webp,
    #[serde(rename = "geotiff", alias = "gtiff", alias = "tiff")]
    GeoTiff,
}

impl OutputFormat {
    pub fn mime_type(&self) -> &'static str {
        match self {
            OutputFormat::Png => "image/png",
            OutputFormat::Jpeg => "image/jpeg",
            OutputFormat::Webp => "image/webp",
            OutputFormat::GeoTiff => "image/tiff; application=geotiff",
        }
    }

    pub fn extension(&self) -> &'static str {
        match self {
            OutputFormat::Png => "png",
            OutputFormat::Jpeg => "jpeg",
            OutputFormat::Webp => "webp",
            OutputFormat::GeoTiff => "tif",
        }
    }

    /// GDAL driver short name (`PNG`, `JPEG`, `WEBP`, `GTiff`).
    pub fn gdal_driver(&self) -> &'static str {
        match self {
            OutputFormat::Png => "PNG",
            OutputFormat::Jpeg => "JPEG",
            OutputFormat::Webp => "WEBP",
            OutputFormat::GeoTiff => "GTiff",
        }
    }
}

/// Georeferencing metadata applied for GeoTIFF (and raw) outputs.
#[derive(Debug, Clone)]
pub struct GeoRef {
    pub crs: Crs,
    pub bbox: Bbox,
    pub nodata: Option<f64>,
}

/// Per-format encoder options. Flattens to GDAL `KEY=VALUE` strings at call time.
#[derive(Debug, Clone)]
pub enum EncodeOpts {
    Png { z_level: u8 },
    Jpeg { quality: u8, progressive: bool },
    Webp { quality: u8, lossless: bool },
    GeoTiff { compress: GtCompress, predictor: u8, tiled: bool },
}

#[derive(Debug, Clone, Copy)]
pub enum GtCompress {
    None,
    Deflate,
    Lzw,
    Zstd,
}

impl EncodeOpts {
    /// Default options per format, tuned for web-tile delivery.
    pub fn defaults_for(format: OutputFormat) -> Self {
        match format {
            OutputFormat::Png => EncodeOpts::Png { z_level: 6 },
            OutputFormat::Jpeg => EncodeOpts::Jpeg { quality: 85, progressive: false },
            OutputFormat::Webp => EncodeOpts::Webp { quality: 80, lossless: false },
            OutputFormat::GeoTiff => EncodeOpts::GeoTiff {
                compress: GtCompress::Deflate,
                predictor: 2,
                tiled: true,
            },
        }
    }
}

/// Encode a pixel buffer to bytes via GDAL `CreateCopy` into `/vsimem/`.
pub fn encode(
    _buffer: &PixelBuffer,
    _georef: Option<&GeoRef>,
    _format: OutputFormat,
    _opts: &EncodeOpts,
) -> Result<Bytes> {
    todo!("phase 2 (encode agent): MEM dataset -> CreateCopy -> /vsimem/ -> bytes")
}
