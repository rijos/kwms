//! Read a rectangular window from a `gdal::Dataset`.
//!
//! There is one path for every request: build a MEM destination of the
//! requested size, georeferenced to the requested bbox and CRS, and let
//! `GDALWarp` fill it (see [`super::raster::warp`]). GDAL handles the
//! geometry — same- or cross-CRS, windows partly or entirely outside the
//! dataset, overview selection — and applies the layer's resampling kernel
//! and nodata. Pixels outside the source come back as nodata (or 0 when the
//! source has none).

use anyhow::{anyhow, Context, Result};
use gdal::Dataset;

use super::raster::{create_georeferenced_mem, pixel_type_from_gdal, read_pixel_buffer, warp, WarpOptions};
use crate::source::{PixelBuffer, WindowRequest};

/// Read (warping as needed) `req.bbox` in `req.crs` at `req.width`×`req.height`.
pub fn read_window(dataset: &Dataset, req: &WindowRequest) -> Result<PixelBuffer> {
    let bands = u32::try_from(dataset.raster_count()).context("raster band count overflow u32")?;
    if bands == 0 {
        return Err(anyhow!("dataset has zero raster bands"));
    }
    // Pixel type from band 1 (kwms serves homogeneously typed datasets).
    let band1 = dataset.rasterband(1).context("failed to read band 1 for dtype probe")?;
    let pixel_type = pixel_type_from_gdal(band1.band_type())?;
    let nodata = req.nodata.or_else(|| band1.no_data_value());

    let mut dst = create_georeferenced_mem(
        pixel_type, req.width, req.height, bands, &req.crs, &req.bbox, nodata,
    )?;
    let opts = WarpOptions {
        resampling: req.resampling,
        src_nodata: req.nodata,
        dst_nodata: nodata,
    };
    warp(&[dataset], &mut dst, &opts).context("warping source window")?;
    read_pixel_buffer(&dst, pixel_type, nodata)
}
