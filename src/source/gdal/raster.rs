//! Shared GDAL raster plumbing: MEM datasets, geotransforms, band
//! (de-)interleaving, `/vsimem/` paths, and the one warp entry point.
//!
//! Everything that moves pixels between GDAL datasets and [`PixelBuffer`]s
//! goes through here — source reads, WMS compositing, and encoding — so the
//! conventions (north-up geotransform, band-interleaved native-endian
//! buffers, nodata handling) are defined exactly once.

use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use gdal::cpl::CslStringList;
use gdal::raster::{Buffer, GdalDataType, GdalType, ResampleAlg};
use gdal::spatial_ref::SpatialRef;
use gdal::{Dataset, DriverManager};

use crate::crs::{Bbox, Crs};
use crate::ffi;
use crate::source::{to_ne_bytes, NeBytes, PixelBuffer, PixelType};

/// Map a GDAL band type to [`PixelType`]. 64-bit integers, complex types,
/// etc. are not served.
pub(crate) fn pixel_type_from_gdal(dt: GdalDataType) -> Result<PixelType> {
    Ok(match dt {
        GdalDataType::UInt8 => PixelType::U8,
        GdalDataType::UInt16 => PixelType::U16,
        GdalDataType::Int16 => PixelType::I16,
        GdalDataType::UInt32 => PixelType::U32,
        GdalDataType::Int32 => PixelType::I32,
        GdalDataType::Float32 => PixelType::F32,
        GdalDataType::Float64 => PixelType::F64,
        other => bail!("unsupported GDAL data type: {other}"),
    })
}

fn gdal_type(pt: PixelType) -> GdalDataType {
    match pt {
        PixelType::U8 => GdalDataType::UInt8,
        PixelType::U16 => GdalDataType::UInt16,
        PixelType::I16 => GdalDataType::Int16,
        PixelType::U32 => GdalDataType::UInt32,
        PixelType::I32 => GdalDataType::Int32,
        PixelType::F32 => GdalDataType::Float32,
        PixelType::F64 => GdalDataType::Float64,
    }
}

/// Bytes per sample.
pub(crate) fn pixel_type_bytes(pt: PixelType) -> usize {
    usize::from(gdal_type(pt).bytes())
}

/// North-up geotransform placing `width`×`height` pixels exactly on `bbox`.
///
/// Refuses a geotransform GDAL can't use faithfully: non-finite values, or
/// a pixel size that is zero, negative or subnormal (GDAL then can't invert
/// it). Request validation keeps client BBOXes out of that range, so this
/// failing is a server bug, reported as an internal error.
pub(crate) fn bbox_to_geotransform(bbox: &Bbox, width: u32, height: u32) -> Result<[f64; 6]> {
    let pixel_width = (bbox.maxx - bbox.minx) / f64::from(width);
    let pixel_height = (bbox.maxy - bbox.miny) / f64::from(height);
    let gt = [bbox.minx, pixel_width, 0.0, bbox.maxy, 0.0, -pixel_height];
    let usable = |v: f64| v.is_normal() && v > 0.0;
    if !(gt.iter().all(|v| v.is_finite()) && usable(pixel_width) && usable(pixel_height)) {
        bail!("internal: unusable geotransform {gt:?} for a {width}x{height} raster");
    }
    Ok(gt)
}

/// A unique `/vsimem/` path (process-wide counter).
pub(crate) fn vsimem_path(prefix: &str, ext: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("/vsimem/kwms-{prefix}-{id}.{ext}")
}

/// Create an empty MEM dataset.
pub(crate) fn create_mem(pt: PixelType, width: u32, height: u32, bands: u32) -> Result<Dataset> {
    let driver = DriverManager::get_driver_by_name("MEM").context("MEM driver not available")?;
    let (w, h, b) = (width as usize, height as usize, bands as usize);
    let ds = match pt {
        PixelType::U8 => driver.create_with_band_type::<u8, _>("", w, h, b),
        PixelType::U16 => driver.create_with_band_type::<u16, _>("", w, h, b),
        PixelType::I16 => driver.create_with_band_type::<i16, _>("", w, h, b),
        PixelType::U32 => driver.create_with_band_type::<u32, _>("", w, h, b),
        PixelType::I32 => driver.create_with_band_type::<i32, _>("", w, h, b),
        PixelType::F32 => driver.create_with_band_type::<f32, _>("", w, h, b),
        PixelType::F64 => driver.create_with_band_type::<f64, _>("", w, h, b),
    };
    ds.context("failed to create MEM dataset")
}

/// Create a MEM dataset georeferenced to `bbox` in `crs`. With `nodata`,
/// every band gets that nodata value and is pre-filled with it (so pixels
/// no warp source covers read as nodata); otherwise bands start at 0.
pub(crate) fn create_georeferenced_mem(
    pt: PixelType,
    width: u32,
    height: u32,
    bands: u32,
    crs: &Crs,
    bbox: &Bbox,
    nodata: Option<f64>,
) -> Result<Dataset> {
    let mut ds = create_mem(pt, width, height, bands)?;
    ds.set_spatial_ref(&gis_order_spatial_ref(crs)?)
        .context("failed to set spatial ref on MEM dataset")?;
    ds.set_geo_transform(&bbox_to_geotransform(bbox, width, height)?)
        .context("failed to set geotransform on MEM dataset")?;
    set_nodata(&ds, nodata)?;
    if let Some(nd) = nodata {
        for b in 1..=ds.raster_count() {
            ds.rasterband(b)
                .with_context(|| format!("failed to fetch band {b}"))?
                .fill(nd, None)
                .with_context(|| format!("failed to fill band {b} with nodata"))?;
        }
    }
    Ok(ds)
}

/// A `SpatialRef` whose data axes are x/y (easting/longitude first), the
/// order every kwms geotransform and `Bbox` uses. GDAL's default for a
/// freshly parsed CRS is the *authority* order (lat/lon for EPSG:4326, N/E
/// for EPSG:3035); attaching that to a dataset would make GDAL read our x/y
/// geotransform as y/x.
pub(crate) fn gis_order_spatial_ref(crs: &Crs) -> Result<SpatialRef> {
    let mut sr = SpatialRef::from_definition(crs.as_str())
        .with_context(|| format!("invalid CRS '{crs}'"))?;
    sr.set_axis_mapping_strategy(gdal::spatial_ref::AxisMappingStrategy::TraditionalGisOrder);
    Ok(sr)
}

/// Set (or leave unset) the nodata value on every band of `ds`.
pub(crate) fn set_nodata(ds: &Dataset, nodata: Option<f64>) -> Result<()> {
    if nodata.is_none() {
        return Ok(());
    }
    for b in 1..=ds.raster_count() {
        ds.rasterband(b)
            .with_context(|| format!("failed to fetch band {b}"))?
            .set_no_data_value(nodata)
            .with_context(|| format!("failed to set nodata on band {b}"))?;
    }
    Ok(())
}

/// Read every band of `ds` into a band-interleaved [`PixelBuffer`].
pub(crate) fn read_pixel_buffer(
    ds: &Dataset,
    pt: PixelType,
    nodata: Option<f64>,
) -> Result<PixelBuffer> {
    let (w, h) = ds.raster_size();
    let bands = ds.raster_count();
    let mut planes = Vec::with_capacity(bands);
    for b in 1..=bands {
        let band = ds.rasterband(b).with_context(|| format!("failed to fetch band {b}"))?;
        planes.push(match pt {
            PixelType::U8 => read_band::<u8>(&band)?,
            PixelType::U16 => read_band::<u16>(&band)?,
            PixelType::I16 => read_band::<i16>(&band)?,
            PixelType::U32 => read_band::<u32>(&band)?,
            PixelType::I32 => read_band::<i32>(&band)?,
            PixelType::F32 => read_band::<f32>(&band)?,
            PixelType::F64 => read_band::<f64>(&band)?,
        });
    }
    Ok(PixelBuffer {
        width: u32::try_from(w).context("raster width overflows u32")?,
        height: u32::try_from(h).context("raster height overflows u32")?,
        bands: u32::try_from(bands).context("band count overflows u32")?,
        pixel_type: pt,
        data: Bytes::from(interleave(&planes, pixel_type_bytes(pt))?),
        nodata,
        alpha: None,
    })
}

fn read_band<T: GdalType + NeBytes>(band: &gdal::raster::RasterBand) -> Result<Vec<u8>> {
    let buf: Buffer<T> = band.read_band_as::<T>().context("band read failed")?;
    let (_, vec) = buf.into_shape_and_vec();
    Ok(to_ne_bytes(vec))
}

/// Planar per-band byte buffers → band-interleaved. Written with iterators
/// (no index arithmetic); all planes must have the same length.
fn interleave(planes: &[Vec<u8>], elem: usize) -> Result<Vec<u8>> {
    let plane_len = planes.first().map_or(0, Vec::len);
    if elem == 0 || planes.iter().any(|p| p.len() != plane_len) {
        bail!("internal: inconsistent band planes");
    }
    let total = plane_len
        .checked_mul(planes.len())
        .context("internal: interleaved buffer size overflows usize")?;
    let mut out = Vec::with_capacity(total);
    let mut samples: Vec<_> = planes.iter().map(|p| p.chunks_exact(elem)).collect();
    while let Some(first) = samples.first_mut().and_then(Iterator::next) {
        out.extend_from_slice(first);
        for band in samples.iter_mut().skip(1) {
            out.extend_from_slice(band.next().context("internal: band plane ended early")?);
        }
    }
    Ok(out)
}

/// Move the last band of `buffer` into its alpha plane (8-bit buffers;
/// 16-bit alpha is scaled down).
pub(crate) fn split_alpha(buffer: PixelBuffer) -> Result<PixelBuffer> {
    let elem = pixel_type_bytes(buffer.pixel_type);
    let bands = buffer.bands as usize;
    if bands < 2 {
        bail!("internal: split_alpha needs at least two bands");
    }
    let px = bands.checked_mul(elem).context("pixel size overflows")?;
    let colour = px.saturating_sub(elem);
    let n = buffer.data.len().checked_div(px).unwrap_or(0);
    let mut data = Vec::with_capacity(n.saturating_mul(colour));
    let mut alpha = Vec::with_capacity(n);
    for pixel in buffer.data.chunks_exact(px) {
        let (c, a) = pixel.split_at(colour);
        data.extend_from_slice(c);
        alpha.push(match buffer.pixel_type {
            PixelType::U16 => {
                let v = a.get(..2).and_then(|b| <[u8; 2]>::try_from(b).ok()).map_or(0, u16::from_ne_bytes);
                u8::try_from(v.checked_div(257).unwrap_or(0)).unwrap_or(u8::MAX)
            }
            _ => a.first().copied().unwrap_or(0),
        });
    }
    Ok(PixelBuffer {
        bands: buffer.bands.saturating_sub(1),
        data: Bytes::from(data),
        alpha: Some(Bytes::from(alpha)),
        ..buffer
    })
}

/// Write a band-interleaved [`PixelBuffer`] into a new MEM dataset.
pub(crate) fn mem_from_pixel_buffer(buffer: &PixelBuffer) -> Result<Dataset> {
    let elem = pixel_type_bytes(buffer.pixel_type);
    let (w, h, bands) = (buffer.width as usize, buffer.height as usize, buffer.bands as usize);
    let plane_bytes = w
        .checked_mul(h)
        .and_then(|px| px.checked_mul(elem))
        .context("PixelBuffer dimensions overflow usize")?;
    let expected = plane_bytes
        .checked_mul(bands)
        .context("PixelBuffer dimensions overflow usize")?;
    if buffer.data.len() != expected {
        bail!(
            "PixelBuffer.data length {} does not match expected {expected} (w={w} h={h} b={bands} elem={elem})",
            buffer.data.len()
        );
    }
    let ds = create_mem(buffer.pixel_type, buffer.width, buffer.height, buffer.bands)?;
    for (b, band_no) in (1..=bands).enumerate() {
        let mut plane = Vec::with_capacity(plane_bytes);
        for sample in buffer.data.chunks_exact(elem).skip(b).step_by(bands) {
            plane.extend_from_slice(sample);
        }
        let mut band = ds
            .rasterband(band_no)
            .with_context(|| format!("failed to fetch band {band_no}"))?;
        match buffer.pixel_type {
            PixelType::U8 => write_band::<u8>(&mut band, &plane, w, h)?,
            PixelType::U16 => write_band::<u16>(&mut band, &plane, w, h)?,
            PixelType::I16 => write_band::<i16>(&mut band, &plane, w, h)?,
            PixelType::U32 => write_band::<u32>(&mut band, &plane, w, h)?,
            PixelType::I32 => write_band::<i32>(&mut band, &plane, w, h)?,
            PixelType::F32 => write_band::<f32>(&mut band, &plane, w, h)?,
            PixelType::F64 => write_band::<f64>(&mut band, &plane, w, h)?,
        }
    }
    Ok(ds)
}

fn write_band<T: GdalType + Copy + FromNeBytes>(
    band: &mut gdal::raster::RasterBand,
    plane: &[u8],
    w: usize,
    h: usize,
) -> Result<()> {
    let values: Vec<T> = plane.chunks_exact(std::mem::size_of::<T>()).map(T::from_ne).collect();
    let mut buf = Buffer::new((w, h), values);
    band.write::<T>((0, 0), (w, h), &mut buf).context("band write failed")
}

trait FromNeBytes: Sized {
    fn from_ne(bytes: &[u8]) -> Self;
}

macro_rules! impl_from_ne_bytes {
    ($($t:ty),*) => {$(
        impl FromNeBytes for $t {
            fn from_ne(bytes: &[u8]) -> Self {
                <$t>::from_ne_bytes(bytes.try_into().expect("chunk size matches type"))
            }
        }
    )*};
}
impl_from_ne_bytes!(u8, u16, i16, u32, i32, f32, f64);

/// How [`warp`] resamples and treats nodata.
#[derive(Debug, Clone, Copy)]
pub(crate) struct WarpOptions {
    pub resampling: ResampleAlg,
    /// Source nodata override; `None` uses the sources' own band nodata.
    pub src_nodata: Option<f64>,
    /// Destination nodata, passed as `-dstnodata`. The destination must
    /// already be initialised (see [`create_georeferenced_mem`]): `warp`
    /// deliberately sets no `INIT_DEST`, because GDALWarp re-applies it per
    /// source and a multi-source mosaic would erase all but the last tile.
    pub dst_nodata: Option<f64>,
    /// The sources' last band is alpha (`-srcalpha`).
    pub src_alpha: bool,
    /// The destination's last band receives alpha (`-dstalpha`): 255 where
    /// a source pixel landed, 0 elsewhere.
    pub dst_alpha: bool,
    /// Single-source reads of a tile index: pure black (0 in every band) is
    /// a gap between tiles, excluded from interpolation (`-srcnodata 0` with
    /// `UNIFIED_SRC_NODATA`), without an output nodata. Ignored when
    /// `src_nodata` is set.
    pub black_is_gap: bool,
}

/// The `gdalwarp -r` token for a resampling kernel. The warper has no Gauss
/// kernel, so config validation rejects `gauss` up front.
pub(crate) fn warp_resampling_name(alg: ResampleAlg) -> Result<&'static str> {
    Ok(match alg {
        ResampleAlg::NearestNeighbour => "near",
        ResampleAlg::Bilinear => "bilinear",
        ResampleAlg::Cubic => "cubic",
        ResampleAlg::CubicSpline => "cubicspline",
        ResampleAlg::Lanczos => "lanczos",
        ResampleAlg::Average => "average",
        ResampleAlg::Mode => "mode",
        ResampleAlg::Gauss => bail!("resampling `gauss` is not supported by the GDAL warper"),
    })
}

/// Warp `sources` into the existing dataset `dst` with `GDALWarp` (the
/// `gdalwarp` library API). GDAL does all the geometry: source/destination
/// window computation, partial and zero overlap, same- or cross-CRS,
/// overview selection (`-ovr AUTO`), resampling, and nodata masking.
///
/// The raw calls, their memory contract and side effects are in
/// `crate::ffi` and documented in `docs/src/unsafe-audit.md` (F5–F7).
pub(crate) fn warp(sources: &[&Dataset], dst: &mut Dataset, opts: &WarpOptions) -> Result<()> {
    let mut args = CslStringList::new();
    let mut push = |s: &str| args.add_string(s).map_err(|e| anyhow!("warp option `{s}`: {e}"));
    push("-r")?;
    push(warp_resampling_name(opts.resampling)?)?;
    push("-ovr")?;
    push("AUTO")?;
    if let Some(nd) = opts.src_nodata {
        push("-srcnodata")?;
        push(&nd.to_string())?;
    }
    if let Some(nd) = opts.dst_nodata {
        push("-dstnodata")?;
        push(&nd.to_string())?;
    }
    if opts.black_is_gap && opts.src_nodata.is_none() {
        for a in ["-srcnodata", "0", "-dstnodata", "None", "-wo", "UNIFIED_SRC_NODATA=YES", "-wo", "INIT_DEST=0"] {
            push(a)?;
        }
    }
    if opts.src_alpha {
        push("-srcalpha")?;
    }
    if opts.dst_alpha {
        push("-dstalpha")?;
    }
    let options = ffi::WarpAppOptions::new(&args)?;
    ffi::gdal_warp(sources, dst, &options)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 4x4 source, values 1..=16 row-major, one band, georeferenced to the
    /// left half of a 3857 box.
    fn source(nodata: Option<f64>) -> Dataset {
        let buf = PixelBuffer {
            width: 4,
            height: 4,
            bands: 1,
            pixel_type: PixelType::U8,
            data: Bytes::from((1u8..=16).collect::<Vec<_>>()),
            nodata,
            alpha: None,
        };
        let mut ds = mem_from_pixel_buffer(&buf).unwrap();
        let sr = SpatialRef::from_epsg(3857).unwrap();
        ds.set_spatial_ref(&sr).unwrap();
        ds.set_geo_transform(&bbox_to_geotransform(&Bbox::from([0.0, 0.0, 400.0, 400.0]), 4, 4).unwrap())
            .unwrap();
        set_nodata(&ds, nodata).unwrap();
        ds
    }

    #[test]
    fn pixel_buffer_roundtrips_through_mem() {
        let buf = PixelBuffer {
            width: 2,
            height: 1,
            bands: 3,
            pixel_type: PixelType::U16,
            data: Bytes::from(to_ne_bytes(vec![1u16, 2, 3, 4, 5, 6])),
            nodata: None,
            alpha: None,
        };
        let ds = mem_from_pixel_buffer(&buf).unwrap();
        let back = read_pixel_buffer(&ds, PixelType::U16, None).unwrap();
        assert_eq!(back.data, buf.data);
    }

    #[test]
    fn warp_nearest_preserves_values_and_fills_outside_with_nodata() {
        let src = source(None);
        // Destination: twice as wide as the source, source on the left half.
        let mut dst = create_georeferenced_mem(
            PixelType::U8, 8, 4, 1, &Crs::new("EPSG:3857"),
            &Bbox::from([0.0, 0.0, 800.0, 400.0]), Some(255.0),
        )
        .unwrap();
        let opts = WarpOptions {
            resampling: ResampleAlg::NearestNeighbour,
            src_nodata: None,
            dst_nodata: Some(255.0), src_alpha: false, dst_alpha: false, black_is_gap: false
        };
        warp(&[&src], &mut dst, &opts).unwrap();
        let out = read_pixel_buffer(&dst, PixelType::U8, Some(255.0)).unwrap();
        // Row 0: source values 1..4, then nodata beyond the source edge.
        assert_eq!(&out.data[..8], &[1, 2, 3, 4, 255, 255, 255, 255]);
    }

    #[test]
    fn warp_honours_nearest_neighbour_on_downsample() {
        // Bilinear/average would invent values that don't exist in the source.
        let src = source(None);
        let mut dst = create_georeferenced_mem(
            PixelType::U8, 2, 2, 1, &Crs::new("EPSG:3857"),
            &Bbox::from([0.0, 0.0, 400.0, 400.0]), None,
        )
        .unwrap();
        let opts = WarpOptions { resampling: ResampleAlg::NearestNeighbour, src_nodata: None, dst_nodata: None , src_alpha: false, dst_alpha: false, black_is_gap: false};
        warp(&[&src], &mut dst, &opts).unwrap();
        let out = read_pixel_buffer(&dst, PixelType::U8, None).unwrap();
        assert!(out.data.iter().all(|v| (1..=16).contains(v)), "{:?}", out.data);
    }

    #[test]
    fn warp_cross_crs_uses_source_nodata() {
        let src = source(Some(7.0));
        let mut dst = create_georeferenced_mem(
            PixelType::U8, 4, 4, 1, &Crs::new("EPSG:4326"),
            &Bbox::from([-180.0, -85.0, 180.0, 85.0]), Some(7.0),
        )
        .unwrap();
        let opts = WarpOptions { resampling: ResampleAlg::Bilinear, src_nodata: None, dst_nodata: Some(7.0) , src_alpha: false, dst_alpha: false, black_is_gap: false};
        warp(&[&src], &mut dst, &opts).unwrap();
        // The tiny source covers almost nothing of the world: everything is nodata.
        let out = read_pixel_buffer(&dst, PixelType::U8, Some(7.0)).unwrap();
        assert!(out.data.iter().all(|&v| v == 7), "{:?}", out.data);
    }

    #[test]
    fn gauss_is_rejected_for_warping() {
        assert!(warp_resampling_name(ResampleAlg::Gauss).is_err());
        assert_eq!(warp_resampling_name(ResampleAlg::NearestNeighbour).unwrap(), "near");
    }

    #[test]
    fn geographic_destination_is_not_axis_swapped() {
        // World source in EPSG:4326, constant 77; a lon/lat window inside it
        // must come back entirely 77. With authority (lat/lon) axis order on
        // the destination, GDAL reads our x/y geotransform as y/x and half the
        // window falls outside the source.
        let buf = PixelBuffer::filled(360, 180, 1, PixelType::U8, 77.0, None).unwrap();
        let mut src = mem_from_pixel_buffer(&buf).unwrap();
        src.set_spatial_ref(&gis_order_spatial_ref(&Crs::new("EPSG:4326")).unwrap()).unwrap();
        src.set_geo_transform(&bbox_to_geotransform(&Bbox::from([-180.0, -90.0, 180.0, 90.0]), 360, 180).unwrap())
            .unwrap();
        for crs in ["EPSG:4326", "CRS:84"] {
            let mut dst = create_georeferenced_mem(
                PixelType::U8, 16, 16, 1, &Crs::new(crs), &Bbox::from([-10.0, 40.0, 10.0, 60.0]), None,
            )
            .unwrap();
            let opts = WarpOptions { resampling: ResampleAlg::NearestNeighbour, src_nodata: None, dst_nodata: None , src_alpha: false, dst_alpha: false, black_is_gap: false};
            warp(&[&src], &mut dst, &opts).unwrap();
            let out = read_pixel_buffer(&dst, PixelType::U8, None).unwrap();
            assert!(out.data.iter().all(|&v| v == 77), "{crs}: {:?}", &out.data[..16]);
        }
    }

    #[test]
    fn multi_source_warp_mosaics_instead_of_overwriting() {
        // Two adjacent 4326 tiles (west, east), both 77: the mosaic across
        // the antimeridian-free middle must be 77 on both sides.
        let tile = |minx: f64| {
            let mut ds = mem_from_pixel_buffer(&PixelBuffer::filled(16, 16, 1, PixelType::U8, 77.0, None).unwrap()).unwrap();
            ds.set_spatial_ref(&gis_order_spatial_ref(&Crs::new("EPSG:4326")).unwrap()).unwrap();
            ds.set_geo_transform(&bbox_to_geotransform(&Bbox::from([minx, -90.0, minx + 180.0, 90.0]), 16, 16).unwrap())
                .unwrap();
            ds
        };
        let (west, east) = (tile(-180.0), tile(0.0));
        for nodata in [None, Some(255.0)] {
            let mut dst = create_georeferenced_mem(
                PixelType::U8, 34, 16, 1, &Crs::new("EPSG:4326"),
                &Bbox::from([-170.0, -80.0, 170.0, 80.0]), nodata,
            )
            .unwrap();
            let opts = WarpOptions { resampling: ResampleAlg::NearestNeighbour, src_nodata: None, dst_nodata: nodata , src_alpha: false, dst_alpha: false, black_is_gap: false};
            warp(&[&west, &east], &mut dst, &opts).unwrap();
            let out = read_pixel_buffer(&dst, PixelType::U8, nodata).unwrap();
            assert!(out.data.iter().all(|&v| v == 77), "nodata={nodata:?}: {:?}", &out.data[..34]);
        }
    }

    fn bad_source(geotransform: Option<[f64; 6]>) -> Dataset {
        let mut ds = mem_from_pixel_buffer(&PixelBuffer::filled(4, 4, 1, PixelType::U8, 9.0, None).unwrap()).unwrap();
        ds.set_spatial_ref(&gis_order_spatial_ref(&Crs::new("EPSG:3857")).unwrap()).unwrap();
        if let Some(gt) = geotransform {
            ds.set_geo_transform(&gt).unwrap();
        }
        ds
    }

    #[test]
    fn failed_warps_never_release_or_leak_the_destination() {
        let mut dst = create_georeferenced_mem(
            PixelType::U8, 4, 4, 1, &Crs::new("EPSG:3857"), &Bbox::from([0.0, 0.0, 400.0, 400.0]), None,
        )
        .unwrap();
        let opts = WarpOptions { resampling: ResampleAlg::NearestNeighbour, src_nodata: None, dst_nodata: None , src_alpha: false, dst_alpha: false, black_is_gap: false};
        assert_eq!(ffi::dataset_ref_count(&dst), 1);
        // Zero pixel size ("Cannot invert geotransform") and no geotransform
        // ("Unable to compute a transformation") make GDALWarp itself fail.
        let zero_pixel = bad_source(Some([0.0, 0.0, 0.0, 400.0, 0.0, 0.0]));
        let no_geotransform = bad_source(None);
        for _ in 0..100 {
            for bad in [&zero_pixel, &no_geotransform] {
                let err = warp(&[bad], &mut dst, &opts).unwrap_err();
                assert!(err.to_string().starts_with("GDALWarp failed: "), "{err:#}");
                assert_eq!(ffi::dataset_ref_count(&dst), 1, "GDALWarp must balance its reference on failure");
            }
        }
        // Still fully usable afterwards, and a success balances too.
        warp(&[&source(None)], &mut dst, &opts).unwrap();
        assert_eq!(ffi::dataset_ref_count(&dst), 1, "GDALWarp must balance its reference on success");
        let out = read_pixel_buffer(&dst, PixelType::U8, None).unwrap();
        assert_eq!(&out.data[..4], &[1, 2, 3, 4]);
    }

    #[test]
    fn rejected_options_allocate_nothing_and_report_an_error() {
        let mut args = CslStringList::new();
        for a in ["-r", "no-such-kernel"] {
            args.add_string(a).unwrap();
        }
        let err = ffi::WarpAppOptions::new(&args).err().expect("GDAL must reject the options");
        assert!(err.to_string().contains("rejected the options"), "{err:#}");
    }

    #[test]
    fn last_cpl_error_only_reports_the_current_call() {
        // Leave a stale error on this thread, then reset: it must not be reported.
        let _ = warp(&[&bad_source(None)], &mut create_mem(PixelType::U8, 1, 1, 1).unwrap(), &WarpOptions {
            resampling: ResampleAlg::NearestNeighbour,
            src_nodata: None,
            dst_nodata: None, src_alpha: false, dst_alpha: false, black_is_gap: false
        });
        ffi::cpl_error_reset();
        assert_eq!(ffi::cpl_error_message(), None);
    }

    /// GDAL is re-entrant: independent datasets may be used from many threads
    /// at once. 16 threads warp concurrently, each with its own source and
    /// destination; every result must equal the single-threaded result.
    #[test]
    fn concurrent_warps_on_independent_datasets_agree() {
        let expected = {
            let mut dst = create_georeferenced_mem(
                PixelType::U8, 8, 4, 1, &Crs::new("EPSG:3857"), &Bbox::from([0.0, 0.0, 800.0, 400.0]), Some(255.0),
            )
            .unwrap();
            let opts = WarpOptions { resampling: ResampleAlg::NearestNeighbour, src_nodata: None, dst_nodata: Some(255.0) , src_alpha: false, dst_alpha: false, black_is_gap: false};
            warp(&[&source(None)], &mut dst, &opts).unwrap();
            read_pixel_buffer(&dst, PixelType::U8, None).unwrap().data
        };
        let handles: Vec<_> = (0..16)
            .map(|_| {
                let expected = expected.clone();
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        let src = source(None);
                        let mut dst = create_georeferenced_mem(
                            PixelType::U8, 8, 4, 1, &Crs::new("EPSG:3857"), &Bbox::from([0.0, 0.0, 800.0, 400.0]), Some(255.0),
                        )
                        .unwrap();
                        let opts = WarpOptions { resampling: ResampleAlg::NearestNeighbour, src_nodata: None, dst_nodata: Some(255.0) , src_alpha: false, dst_alpha: false, black_is_gap: false};
                        warp(&[&src], &mut dst, &opts).unwrap();
                        assert_eq!(read_pixel_buffer(&dst, PixelType::U8, None).unwrap().data, expected);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().expect("a warping thread panicked");
        }
    }

    /// GDAL's error state is per thread: errors raised continuously on other
    /// threads never show up in this thread's reset-then-read window.
    #[test]
    fn cpl_error_state_is_isolated_per_thread() {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let noisy: Vec<_> = (0..4)
            .map(|_| {
                let stop = stop.clone();
                std::thread::spawn(move || {
                    let opts = WarpOptions { resampling: ResampleAlg::NearestNeighbour, src_nodata: None, dst_nodata: None , src_alpha: false, dst_alpha: false, black_is_gap: false};
                    let mut dst = create_mem(PixelType::U8, 1, 1, 1).unwrap();
                    while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                        let _ = warp(&[&bad_source(None)], &mut dst, &opts); // raises CPL errors
                    }
                })
            })
            .collect();
        for _ in 0..2000 {
            ffi::cpl_error_reset();
            assert_eq!(ffi::cpl_error_message(), None, "another thread's GDAL error leaked into this thread");
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for t in noisy {
            t.join().unwrap();
        }
    }
}
