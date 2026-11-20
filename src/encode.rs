//! Output encoding — GDAL-backed for PNG / JPEG / WebP / GeoTIFF.
//!
//! See `docs/src/output-encoding.md`. One of three modules permitted to call
//! into the `gdal` crate directly (alongside `crs/gdal.rs` and `source/gdal/`).

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use gdal::raster::RasterCreationOptions;
use gdal::vsi::get_vsi_mem_file_bytes_owned;
use gdal::DriverManager;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::crs::{Bbox, Crs};
use crate::source::gdal::raster::{
    bbox_to_geotransform, mem_from_pixel_buffer, remove_mem_file, set_nodata, vsimem_path,
};
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
    Png {
        z_level: u8,
    },
    Jpeg {
        quality: u8,
        progressive: bool,
    },
    Webp {
        quality: u8,
        lossless: bool,
    },
    GeoTiff {
        compress: GtCompress,
        predictor: u8,
        tiled: bool,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum GtCompress {
    None,
    Deflate,
    Lzw,
    Zstd,
}

impl GtCompress {
    fn as_str(self) -> &'static str {
        match self {
            GtCompress::None => "NONE",
            GtCompress::Deflate => "DEFLATE",
            GtCompress::Lzw => "LZW",
            GtCompress::Zstd => "ZSTD",
        }
    }
}

impl EncodeOpts {
    /// The configured settings for `format`.
    pub fn from_config(format: OutputFormat, cfg: &crate::config::OutputFormatsConfig) -> Self {
        use crate::config::GeoTiffCompression as C;
        match format {
            OutputFormat::Png => EncodeOpts::Png {
                z_level: cfg.png.compression,
            },
            OutputFormat::Jpeg => EncodeOpts::Jpeg {
                quality: cfg.jpeg.quality,
                progressive: cfg.jpeg.progressive,
            },
            OutputFormat::Webp => EncodeOpts::Webp {
                quality: cfg.webp.quality,
                lossless: cfg.webp.lossless,
            },
            OutputFormat::GeoTiff => EncodeOpts::GeoTiff {
                compress: match cfg.geotiff.compress {
                    C::None => GtCompress::None,
                    C::Deflate => GtCompress::Deflate,
                    C::Lzw => GtCompress::Lzw,
                    C::Zstd => GtCompress::Zstd,
                },
                predictor: cfg.geotiff.predictor,
                tiled: cfg.geotiff.tiled,
            },
        }
    }

    /// Default options per format, tuned for web-tile delivery (the same as
    /// an empty `output_formats:` config).
    pub fn defaults_for(format: OutputFormat) -> Self {
        match format {
            OutputFormat::Png => EncodeOpts::Png { z_level: 6 },
            OutputFormat::Jpeg => EncodeOpts::Jpeg {
                quality: 85,
                progressive: false,
            },
            OutputFormat::Webp => EncodeOpts::Webp {
                quality: 80,
                lossless: false,
            },
            OutputFormat::GeoTiff => EncodeOpts::GeoTiff {
                compress: GtCompress::Deflate,
                predictor: 2,
                tiled: true,
            },
        }
    }

    fn to_kv_strings(&self) -> Vec<String> {
        match self {
            EncodeOpts::Png { z_level } => vec![format!("ZLEVEL={z_level}")],
            EncodeOpts::Jpeg { quality, progressive } => vec![
                format!("QUALITY={quality}"),
                format!("PROGRESSIVE={}", if *progressive { "YES" } else { "NO" }),
            ],
            EncodeOpts::Webp { quality, lossless } => vec![
                format!("QUALITY={quality}"),
                format!("LOSSLESS={}", if *lossless { "TRUE" } else { "FALSE" }),
            ],
            EncodeOpts::GeoTiff {
                compress,
                predictor,
                tiled,
            } => vec![
                format!("COMPRESS={}", compress.as_str()),
                format!("PREDICTOR={predictor}"),
                format!("TILED={}", if *tiled { "YES" } else { "NO" }),
            ],
        }
    }
}

/// Whether a format has an alpha channel. Without one (JPEG), no-data
/// pixels are painted white.
fn supports_alpha(format: OutputFormat) -> bool {
    !matches!(format, OutputFormat::Jpeg)
}

/// Apply `buffer.alpha` for `format`: an extra alpha band where the format
/// has one, otherwise blend onto white. Returns the buffer to write and
/// whether its last band is alpha.
fn apply_alpha(buffer: &PixelBuffer, format: OutputFormat) -> Result<(std::borrow::Cow<'_, PixelBuffer>, bool)> {
    use std::borrow::Cow;
    let Some(alpha) = &buffer.alpha else {
        return Ok((Cow::Borrowed(buffer), false));
    };
    let bands = buffer.bands as usize;
    let pixels = (buffer.width as usize)
        .checked_mul(buffer.height as usize)
        .context("image size overflows")?;
    if alpha.len() != pixels {
        anyhow::bail!("internal: alpha has {} entries for {pixels} pixels", alpha.len());
    }
    let elem = crate::source::gdal::raster::pixel_type_bytes(buffer.pixel_type);
    let px_bytes = bands.checked_mul(elem).context("pixel size overflows")?;
    if supports_alpha(format) {
        // Alpha in the sample type: 8-bit as is, 16-bit scaled; other types
        // have no conventional alpha range, so they keep nodata only.
        let to_sample = |a: u8| -> Option<Vec<u8>> {
            match buffer.pixel_type {
                crate::source::PixelType::U8 => Some(vec![a]),
                crate::source::PixelType::U16 => Some(u16::from(a).saturating_mul(257).to_ne_bytes().to_vec()),
                _ => None,
            }
        };
        if to_sample(0).is_none() {
            return Ok((Cow::Borrowed(buffer), false));
        }
        let mut data = Vec::with_capacity(buffer.data.len().saturating_add(pixels.saturating_mul(elem)));
        for (px, a) in buffer.data.chunks_exact(px_bytes).zip(alpha.iter()) {
            data.extend_from_slice(px);
            data.extend_from_slice(&to_sample(*a).unwrap_or_default());
        }
        let bands = buffer.bands.checked_add(1).context("band count overflows")?;
        let out = PixelBuffer {
            bands,
            data: Bytes::from(data),
            nodata: None,
            alpha: None,
            ..buffer.clone()
        };
        return Ok((Cow::Owned(out), true));
    }
    // No alpha channel: blend onto white (JPEG is 8-bit).
    if buffer.pixel_type != crate::source::PixelType::U8 {
        anyhow::bail!("{format:?} output needs 8-bit data");
    }
    let mut data = buffer.data.to_vec();
    for (px, a) in data.chunks_exact_mut(px_bytes).zip(alpha.iter()) {
        let a = u32::from(*a);
        let white = 255u32.saturating_mul(255u32.saturating_sub(a));
        for v in px.iter_mut() {
            // (v*a + 255*(255-a) + 127) / 255: stays within 0..=255.
            let blended = u32::from(*v)
                .saturating_mul(a)
                .saturating_add(white)
                .saturating_add(127)
                .checked_div(255)
                .unwrap_or(255);
            *v = u8::try_from(blended).unwrap_or(u8::MAX);
        }
    }
    let out = PixelBuffer {
        data: Bytes::from(data),
        nodata: None,
        alpha: None,
        ..buffer.clone()
    };
    Ok((Cow::Owned(out), false))
}

/// Encode a pixel buffer to bytes via GDAL `CreateCopy` into `/vsimem/`.
///
/// Coverage (`buffer.alpha`) becomes an alpha band for PNG, WebP and
/// GeoTIFF (transparent where there is no data) and white for JPEG.
/// Without alpha, `buffer.nodata` is set on every band: GeoTIFF records it,
/// and the PNG driver writes it as a `tRNS` chunk.
pub fn encode(buffer: &PixelBuffer, georef: Option<&GeoRef>, format: OutputFormat, opts: &EncodeOpts) -> Result<Bytes> {
    let (buffer, has_alpha_band) = apply_alpha(buffer, format)?;
    let buffer = buffer.as_ref();
    // 1) Build an in-memory MEM dataset of the appropriate dtype.
    let mut mem = mem_from_pixel_buffer(buffer).context("failed to create in-memory MEM dataset")?;
    if has_alpha_band {
        let last = mem.raster_count();
        mem.rasterband(last)
            .context("alpha band")?
            .set_color_interpretation(gdal::raster::ColorInterpretation::AlphaBand)
            .context("marking the alpha band")?;
    }

    // 2) Georeferencing (GeoTIFF) and nodata (all formats, unless alpha
    //    already says where there is no data).
    if let Some(gr) = georef {
        mem.set_projection(gr.crs.as_str())
            .context("failed to set projection on MEM dataset")?;
        mem.set_geo_transform(&bbox_to_geotransform(&gr.bbox, buffer.width, buffer.height)?)
            .context("failed to set geotransform on MEM dataset")?;
    }
    if !has_alpha_band && buffer.nodata.is_some() {
        set_nodata(&mem, georef.and_then(|g| g.nodata).or(buffer.nodata))?;
    } else if !has_alpha_band {
        set_nodata(&mem, georef.and_then(|g| g.nodata))?;
    }

    // 3) Pick the target driver and assemble its creation options.
    let driver = DriverManager::get_driver_by_name(format.gdal_driver())
        .with_context(|| format!("GDAL driver `{}` not available", format.gdal_driver()))?;
    let kv_strings = opts.to_kv_strings();
    let creation_options = RasterCreationOptions::from_iter(kv_strings.iter().map(|s| s.as_str()));

    // 4) CreateCopy into a unique /vsimem/ path.
    let vsi_path = vsimem_path("encode", format.extension());

    let copy_result = mem.create_copy(&driver, &vsi_path, &creation_options);
    // Always attempt to clean up the /vsimem/ entry, even on error.
    let bytes = match copy_result {
        Ok(out_ds) => {
            // Drop the output dataset before reading its bytes, so GDAL flushes
            // any buffered writes to the in-memory file.
            drop(out_ds);
            let raw = get_vsi_mem_file_bytes_owned(&vsi_path);
            // The bytes are copied out; drop the file and any PAM sidecar
            // (written for nodata on JPEG/WebP).
            remove_mem_file(&vsi_path);
            Bytes::from(raw.context("failed to read encoded bytes from /vsimem/")?)
        }
        Err(e) => {
            remove_mem_file(&vsi_path);
            return Err(anyhow!(e))
                .with_context(|| format!("GDAL CreateCopy failed for driver `{}`", format.gdal_driver()));
        }
    };

    Ok(bytes)
}

/// Compute a standard north-up geotransform from a bbox + raster size.
///
/// `[origin_x, pixel_width, 0, origin_y, 0, -pixel_height]`
#[cfg(test)]
mod tests {
    use super::*;
    use crate::crs::{Bbox, Crs};
    use crate::source::PixelType;
    use bytes::Bytes;
    use gdal::vsi::{create_mem_file, unlink_mem_file};
    use gdal::Dataset;

    fn make_rgba_u8_16x16() -> PixelBuffer {
        let width = 16u32;
        let height = 16u32;
        let bands = 4u32;
        let mut data = Vec::with_capacity((width * height * bands) as usize);
        for row in 0..height {
            for col in 0..width {
                data.push(col as u8 * 16); // R
                data.push(row as u8 * 16); // G
                data.push(((row + col) as u8).wrapping_mul(8)); // B
                data.push(255); // A
            }
        }
        PixelBuffer {
            width,
            height,
            bands,
            pixel_type: PixelType::U8,
            data: Bytes::from(data),
            nodata: None,
            alpha: None,
        }
    }

    #[test]
    fn encodes_png_with_magic_bytes() {
        let buffer = make_rgba_u8_16x16();
        let bytes = encode(
            &buffer,
            None,
            OutputFormat::Png,
            &EncodeOpts::defaults_for(OutputFormat::Png),
        )
        .expect("PNG encode succeeded");
        assert!(bytes.len() > 8, "PNG output too short: {} bytes", bytes.len());
        assert_eq!(&bytes[..4], &[0x89, 0x50, 0x4E, 0x47], "PNG output missing PNG magic");
    }

    /// No-data pixels: an alpha band in PNG, white in JPEG.
    #[test]
    fn alpha_becomes_transparency_or_white() {
        let buffer = PixelBuffer {
            width: 2,
            height: 1,
            bands: 3,
            pixel_type: PixelType::U8,
            data: Bytes::from(vec![10, 20, 30, 0, 0, 0]),
            nodata: None,
            alpha: Some(Bytes::from(vec![255, 0])),
        };
        let decode = |bytes: Bytes, ext: &str| {
            let path = vsimem_path("alpha-test", ext);
            create_mem_file(&path, bytes.to_vec()).unwrap();
            let ds = Dataset::open(&path).unwrap();
            let bands: Vec<Vec<u8>> = (1..=ds.raster_count())
                .map(|b| {
                    ds.rasterband(b)
                        .unwrap()
                        .read_band_as::<u8>()
                        .unwrap()
                        .into_shape_and_vec()
                        .1
                })
                .collect();
            drop(ds);
            let _ = unlink_mem_file(&path);
            bands
        };
        let png = decode(
            encode(
                &buffer,
                None,
                OutputFormat::Png,
                &EncodeOpts::defaults_for(OutputFormat::Png),
            )
            .unwrap(),
            "png",
        );
        assert_eq!(png.len(), 4, "RGBA");
        assert_eq!(png[3], vec![255, 0]);
        assert_eq!((png[0][0], png[1][0], png[2][0]), (10, 20, 30));
        let jpeg = decode(
            encode(
                &buffer,
                None,
                OutputFormat::Jpeg,
                &EncodeOpts::defaults_for(OutputFormat::Jpeg),
            )
            .unwrap(),
            "jpg",
        );
        assert_eq!(jpeg.len(), 3);
        // The no-data pixel is white (JPEG is lossy: allow a little).
        assert!(jpeg.iter().all(|band| band[1] >= 240), "{jpeg:?}");
    }

    #[test]
    fn encodes_jpeg_with_magic_bytes() {
        // JPEG driver does not accept 4-band input; use a 3-band RGB buffer.
        let width = 16u32;
        let height = 16u32;
        let bands = 3u32;
        let mut data = Vec::with_capacity((width * height * bands) as usize);
        for row in 0..height {
            for col in 0..width {
                data.push(col as u8 * 16);
                data.push(row as u8 * 16);
                data.push(((row + col) as u8).wrapping_mul(8));
            }
        }
        let buffer = PixelBuffer {
            width,
            height,
            bands,
            pixel_type: PixelType::U8,
            data: Bytes::from(data),
            nodata: None,
            alpha: None,
        };
        let bytes = encode(
            &buffer,
            None,
            OutputFormat::Jpeg,
            &EncodeOpts::defaults_for(OutputFormat::Jpeg),
        )
        .expect("JPEG encode succeeded");
        assert!(bytes.len() > 3, "JPEG output too short");
        assert_eq!(&bytes[..3], &[0xFF, 0xD8, 0xFF], "JPEG output missing SOI marker");
    }

    #[test]
    fn encodes_geotiff_with_projection() {
        // Single-band f32 buffer is simpler for the GeoTIFF georef test.
        let width = 16u32;
        let height = 16u32;
        let pixel_count = (width * height) as usize;
        let mut data: Vec<u8> = Vec::with_capacity(pixel_count * 4);
        for row in 0..height {
            for col in 0..width {
                let v = (row * width + col) as f32;
                data.extend_from_slice(&v.to_ne_bytes());
            }
        }
        let buffer = PixelBuffer {
            width,
            height,
            bands: 1,
            pixel_type: PixelType::F32,
            data: Bytes::from(data),
            nodata: None,
            alpha: None,
        };
        let georef = GeoRef {
            crs: Crs::new("EPSG:4326"),
            bbox: Bbox::from([0.0, 0.0, 1.0, 1.0]),
            nodata: None,
        };
        let bytes = encode(
            &buffer,
            Some(&georef),
            OutputFormat::GeoTiff,
            &EncodeOpts::defaults_for(OutputFormat::GeoTiff),
        )
        .expect("GeoTIFF encode succeeded");
        assert!(bytes.len() > 8, "GeoTIFF output too short");

        // Round-trip: write the bytes to another /vsimem/ path, open via GDAL,
        // and assert the projection metadata mentions WGS 84 / EPSG:4326.
        let path = vsimem_path("encode-test", "tif");
        create_mem_file(&path, bytes.to_vec()).expect("write vsimem file");
        let ds = Dataset::open(&path).expect("open GeoTIFF from vsimem");
        let projection = ds.projection();
        drop(ds);
        let _ = unlink_mem_file(&path);

        assert!(
            projection.contains("4326") || projection.contains("WGS 84"),
            "projection metadata missing expected token: {projection}"
        );
    }

    #[test]
    fn png_carries_nodata_as_transparency() {
        let buffer = PixelBuffer::filled(4, 4, 3, crate::source::PixelType::U8, 0.0, Some(0.0)).unwrap();
        let bytes = encode(
            &buffer,
            None,
            OutputFormat::Png,
            &EncodeOpts::defaults_for(OutputFormat::Png),
        )
        .expect("png encode");
        let path = vsimem_path("encode-test", "png");
        create_mem_file(&path, bytes.to_vec()).unwrap();
        let ds = Dataset::open(&path).unwrap();
        let nd = ds.rasterband(1).unwrap().no_data_value();
        drop(ds);
        let _ = unlink_mem_file(&path);
        assert_eq!(nd, Some(0.0), "PNG should round-trip nodata via tRNS");
    }
}
