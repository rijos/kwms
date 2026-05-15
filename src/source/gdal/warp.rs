//! Read (and optionally warp) a rectangular window from a `gdal::Dataset`.
//!
//! Strategy:
//!
//! - If the request's CRS matches the dataset's native CRS, we map the bbox
//!   to a pixel window using the dataset's geotransform and call
//!   `read_as::<T>(window, window_size, shape)` per band (GDAL's RasterIO
//!   handles up/down sampling with the requested resampling kernel).
//! - Otherwise we create an in-memory destination dataset of the requested
//!   pixel size, set its CRS and geotransform to match the requested bbox,
//!   call `gdal::raster::warp::reproject` to fill it, then read the bands
//!   back out.
//!
//! In both cases the output `PixelBuffer` is row-major, band-interleaved,
//! matching the convention used in `crate::source::PixelBuffer.data`.

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use gdal::raster::{Buffer, GdalDataType, GdalType, ResampleAlg};
use gdal::spatial_ref::SpatialRef;
use gdal::{Dataset, DriverManager};

use crate::crs::{Bbox, Crs};
use crate::source::{PixelBuffer, PixelType, Resampling, WindowRequest};

/// Convert our public `Resampling` to GDAL's `ResampleAlg`.
fn to_gdal_resampling(r: Resampling) -> ResampleAlg {
    match r {
        Resampling::Nearest => ResampleAlg::NearestNeighbour,
        Resampling::Bilinear => ResampleAlg::Bilinear,
        Resampling::Cubic => ResampleAlg::Cubic,
        Resampling::CubicSpline => ResampleAlg::CubicSpline,
        Resampling::Lanczos => ResampleAlg::Lanczos,
        Resampling::Average => ResampleAlg::Average,
        Resampling::Mode => ResampleAlg::Mode,
    }
}

/// Best-effort mapping from GDAL's data type to our `PixelType`. Any
/// non-supported type (e.g. 64-bit integers, complex) becomes an error.
pub(crate) fn pixel_type_from_gdal(dt: GdalDataType) -> Result<PixelType> {
    Ok(match dt {
        GdalDataType::UInt8 => PixelType::U8,
        GdalDataType::UInt16 => PixelType::U16,
        GdalDataType::Int16 => PixelType::I16,
        GdalDataType::UInt32 => PixelType::U32,
        GdalDataType::Int32 => PixelType::I32,
        GdalDataType::Float32 => PixelType::F32,
        GdalDataType::Float64 => PixelType::F64,
        other => return Err(anyhow!("unsupported GDAL data type: {other}")),
    })
}

/// Number of bytes per element of a `PixelType`.
pub(crate) fn pixel_type_bytes(pt: PixelType) -> usize {
    match pt {
        PixelType::U8 => 1,
        PixelType::U16 | PixelType::I16 => 2,
        PixelType::U32 | PixelType::I32 | PixelType::F32 => 4,
        PixelType::F64 => 8,
    }
}

/// Public entry point: read (with optional warp) a window from `dataset`.
pub fn read_window(
    dataset: &mut Dataset,
    native_crs: &Crs,
    req: &WindowRequest,
) -> Result<PixelBuffer> {
    let bands = dataset.raster_count();
    if bands == 0 {
        return Err(anyhow!("dataset has zero raster bands"));
    }

    // Determine pixel type from band 1 (we assume homogeneous typing across
    // bands, which is true for the formats kwms serves).
    let band1 = dataset
        .rasterband(1)
        .context("failed to read band 1 for dtype probe")?;
    let pixel_type = pixel_type_from_gdal(band1.band_type())?;
    let nodata = band1.no_data_value();
    drop(band1);

    let bands_u32 = u32::try_from(bands).context("raster band count overflow u32")?;
    let same_crs = native_crs.as_str() == req.crs.as_str();

    if same_crs {
        read_native_window(dataset, req, pixel_type, bands_u32, nodata)
    } else {
        warp_window(dataset, req, pixel_type, bands_u32, nodata)
    }
}

/// Compute the (fractional) pixel window for `bbox` against the dataset's
/// geo_transform. Returns `(x_off, y_off, x_size, y_size)` in pixel units.
fn bbox_to_pixel_window(dataset: &Dataset, bbox: &Bbox) -> Result<(isize, isize, usize, usize)> {
    let gt = dataset
        .geo_transform()
        .context("dataset has no geotransform; cannot compute pixel window")?;
    // gt = [ox, dx, rx, oy, ry, dy]; rotation terms (rx, ry) are assumed 0
    // for this code path. If they aren't, GDAL itself would still let us
    // read but the bbox math gets ugly — we explicitly reject rotated grids.
    if gt[2] != 0.0 || gt[4] != 0.0 {
        return Err(anyhow!(
            "rotated/sheared geotransform not supported for native-CRS reads"
        ));
    }
    let (ox, dx, oy, dy) = (gt[0], gt[1], gt[3], gt[5]);

    // Map bbox corners to pixel space. dy is normally negative (north-up).
    let px_min_x = (bbox.minx - ox) / dx;
    let px_max_x = (bbox.maxx - ox) / dx;
    let py_min_y = (bbox.miny - oy) / dy;
    let py_max_y = (bbox.maxy - oy) / dy;

    let x_lo = px_min_x.min(px_max_x);
    let x_hi = px_min_x.max(px_max_x);
    let y_lo = py_min_y.min(py_max_y);
    let y_hi = py_min_y.max(py_max_y);

    let x_off = x_lo.floor() as isize;
    let y_off = y_lo.floor() as isize;
    let x_size = ((x_hi - x_lo).round().max(1.0)) as usize;
    let y_size = ((y_hi - y_lo).round().max(1.0)) as usize;
    Ok((x_off, y_off, x_size, y_size))
}

/// Native-CRS path: read each band, interleave into the output buffer.
fn read_native_window(
    dataset: &mut Dataset,
    req: &WindowRequest,
    pixel_type: PixelType,
    bands: u32,
    nodata: Option<f64>,
) -> Result<PixelBuffer> {
    let (x_off, y_off, x_size, y_size) = bbox_to_pixel_window(dataset, &req.bbox)?;
    let shape = (req.width as usize, req.height as usize);
    let alg = Some(to_gdal_resampling(req.resampling));

    // Per-band planar buffers, in host byte order.
    let mut planes: Vec<Vec<u8>> = Vec::with_capacity(bands as usize);
    for band_idx in 1..=bands as usize {
        let band = dataset
            .rasterband(band_idx)
            .with_context(|| format!("failed to fetch band {band_idx}"))?;
        let plane_bytes: Vec<u8> = match pixel_type {
            PixelType::U8 => read_band_planar::<u8>(&band, x_off, y_off, x_size, y_size, shape, alg)?,
            PixelType::U16 => read_band_planar::<u16>(&band, x_off, y_off, x_size, y_size, shape, alg)?,
            PixelType::I16 => read_band_planar::<i16>(&band, x_off, y_off, x_size, y_size, shape, alg)?,
            PixelType::U32 => read_band_planar::<u32>(&band, x_off, y_off, x_size, y_size, shape, alg)?,
            PixelType::I32 => read_band_planar::<i32>(&band, x_off, y_off, x_size, y_size, shape, alg)?,
            PixelType::F32 => read_band_planar::<f32>(&band, x_off, y_off, x_size, y_size, shape, alg)?,
            PixelType::F64 => read_band_planar::<f64>(&band, x_off, y_off, x_size, y_size, shape, alg)?,
        };
        planes.push(plane_bytes);
    }

    let data = interleave_bands(&planes, req.width, req.height, bands, pixel_type)?;
    Ok(PixelBuffer {
        width: req.width,
        height: req.height,
        bands,
        pixel_type,
        data: Bytes::from(data),
        nodata,
    })
}

fn read_band_planar<T: GdalType + Copy>(
    band: &gdal::raster::RasterBand,
    x_off: isize,
    y_off: isize,
    x_size: usize,
    y_size: usize,
    shape: (usize, usize),
    alg: Option<ResampleAlg>,
) -> Result<Vec<u8>> {
    let buf: Buffer<T> = band
        .read_as::<T>((x_off, y_off), (x_size, y_size), shape, alg)
        .with_context(|| format!("band read_as failed for shape={shape:?}"))?;
    let (_, vec) = buf.into_shape_and_vec();
    typed_vec_to_ne_bytes(vec)
}

/// Reinterpret a `Vec<T>` as native-endian bytes — the same convention
/// `PixelBuffer.data` uses elsewhere in the codebase (see `encode.rs`).
fn typed_vec_to_ne_bytes<T: Copy>(vec: Vec<T>) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(vec.len() * std::mem::size_of::<T>());
    for v in vec {
        // SAFETY: `T: Copy` and we only read the bytes of a local stack
        // variable for the duration of this loop iteration.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                (&v as *const T) as *const u8,
                std::mem::size_of::<T>(),
            )
        };
        out.extend_from_slice(bytes);
    }
    Ok(out)
}

/// Interleave per-band planar byte buffers into the row-major, band-interleaved
/// layout that `PixelBuffer` expects.
fn interleave_bands(
    planes: &[Vec<u8>],
    width: u32,
    height: u32,
    bands: u32,
    pixel_type: PixelType,
) -> Result<Vec<u8>> {
    let elem = pixel_type_bytes(pixel_type);
    let pixels = (width as usize) * (height as usize);
    let plane_bytes = pixels * elem;
    for (i, p) in planes.iter().enumerate() {
        if p.len() != plane_bytes {
            return Err(anyhow!(
                "band {} byte length {} != expected {} (w={} h={} elem={})",
                i + 1,
                p.len(),
                plane_bytes,
                width,
                height,
                elem
            ));
        }
    }
    let mut out = vec![0u8; pixels * (bands as usize) * elem];
    for pix in 0..pixels {
        for band_idx in 0..bands as usize {
            let src = &planes[band_idx][pix * elem..(pix + 1) * elem];
            let dst_off = (pix * bands as usize + band_idx) * elem;
            out[dst_off..dst_off + elem].copy_from_slice(src);
        }
    }
    Ok(out)
}

/// Warping path: build an in-memory destination dataset matching `req`,
/// `GDALReprojectImage` into it, read back.
fn warp_window(
    dataset: &mut Dataset,
    req: &WindowRequest,
    pixel_type: PixelType,
    bands: u32,
    nodata: Option<f64>,
) -> Result<PixelBuffer> {
    let mem_driver = DriverManager::get_driver_by_name("MEM")
        .context("MEM driver not available for warp destination")?;
    let mut dst = create_mem_for_type(&mem_driver, pixel_type, req.width, req.height, bands)?;

    // Set the destination spatial reference and a north-up geotransform that
    // matches the requested bbox + pixel size.
    let dst_sr = SpatialRef::from_definition(req.crs.as_str())
        .with_context(|| format!("invalid target CRS '{}'", req.crs.as_str()))?;
    dst.set_spatial_ref(&dst_sr)
        .context("failed to set spatial ref on warp destination")?;

    let gt = bbox_to_geotransform(&req.bbox, req.width, req.height);
    dst.set_geo_transform(&gt)
        .context("failed to set geotransform on warp destination")?;

    // gdal-0.17's safe wrapper exposes only `reproject(src, dst)` — no
    // resampling-kernel argument; GDAL defaults to bilinear inside that
    // wrapper. We accept that for now (a future pass can drop down to the
    // gdal-sys level if a per-request kernel matters; see notes in
    // `docs/src/source-abstraction.md` on resampling).
    gdal::raster::reproject(dataset, &dst)
        .context("GDAL reproject (warp) failed")?;

    // Read each band of the destination back into a row-major plane.
    let mut planes: Vec<Vec<u8>> = Vec::with_capacity(bands as usize);
    for band_idx in 1..=bands as usize {
        let band = dst
            .rasterband(band_idx)
            .with_context(|| format!("failed to fetch warp dst band {band_idx}"))?;
        let plane: Vec<u8> = match pixel_type {
            PixelType::U8 => read_full_band::<u8>(&band)?,
            PixelType::U16 => read_full_band::<u16>(&band)?,
            PixelType::I16 => read_full_band::<i16>(&band)?,
            PixelType::U32 => read_full_band::<u32>(&band)?,
            PixelType::I32 => read_full_band::<i32>(&band)?,
            PixelType::F32 => read_full_band::<f32>(&band)?,
            PixelType::F64 => read_full_band::<f64>(&band)?,
        };
        planes.push(plane);
    }

    let data = interleave_bands(&planes, req.width, req.height, bands, pixel_type)?;
    Ok(PixelBuffer {
        width: req.width,
        height: req.height,
        bands,
        pixel_type,
        data: Bytes::from(data),
        nodata,
    })
}

fn read_full_band<T: GdalType + Copy>(band: &gdal::raster::RasterBand) -> Result<Vec<u8>> {
    let buf: Buffer<T> = band
        .read_band_as::<T>()
        .context("read_band_as failed on warp destination")?;
    let (_, vec) = buf.into_shape_and_vec();
    typed_vec_to_ne_bytes(vec)
}

fn create_mem_for_type(
    driver: &gdal::Driver,
    pt: PixelType,
    width: u32,
    height: u32,
    bands: u32,
) -> Result<Dataset> {
    let w = width as usize;
    let h = height as usize;
    let b = bands as usize;
    match pt {
        PixelType::U8 => driver.create_with_band_type::<u8, _>("kwms-warp-dst", w, h, b),
        PixelType::U16 => driver.create_with_band_type::<u16, _>("kwms-warp-dst", w, h, b),
        PixelType::I16 => driver.create_with_band_type::<i16, _>("kwms-warp-dst", w, h, b),
        PixelType::U32 => driver.create_with_band_type::<u32, _>("kwms-warp-dst", w, h, b),
        PixelType::I32 => driver.create_with_band_type::<i32, _>("kwms-warp-dst", w, h, b),
        PixelType::F32 => driver.create_with_band_type::<f32, _>("kwms-warp-dst", w, h, b),
        PixelType::F64 => driver.create_with_band_type::<f64, _>("kwms-warp-dst", w, h, b),
    }
    .context("failed to create MEM warp destination")
}

/// `[origin_x, pixel_width, 0, origin_y, 0, -pixel_height]`. The same form
/// `encode::bbox_to_geotransform` uses (kept private here to avoid a
/// cross-module dependency for one tiny helper).
fn bbox_to_geotransform(bbox: &Bbox, width: u32, height: u32) -> [f64; 6] {
    let pixel_width = (bbox.maxx - bbox.minx) / (width as f64);
    let pixel_height = (bbox.maxy - bbox.miny) / (height as f64);
    [bbox.minx, pixel_width, 0.0, bbox.maxy, 0.0, -pixel_height]
}
