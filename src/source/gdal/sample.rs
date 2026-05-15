//! `Source::sample` — read one pixel per band at a given coordinate.
//!
//! Pipeline:
//!
//! 1. Transform `(x, y)` from `crs` into the dataset's native CRS using
//!    `crate::crs::transform_bbox` on a degenerate (zero-area) bbox.
//! 2. Convert to pixel space via the dataset's geotransform.
//! 3. Read a 1×1 window from each band (always with nearest-neighbour —
//!    sampling a single pixel doesn't have a meaningful kernel choice) and
//!    convert the value to `f64` for the `PixelValue` API.

use anyhow::{anyhow, Context, Result};
use gdal::raster::ResampleAlg;
use gdal::Dataset;

use crate::crs::{transform_bbox, Bbox, Coord, Crs};
use crate::source::PixelValue;

pub fn sample(
    dataset: &mut Dataset,
    native_crs: &Crs,
    coord: Coord,
    crs: Crs,
) -> Result<Vec<PixelValue>> {
    // Step 1: get the coordinate in the dataset's native CRS.
    let (nx, ny) = if crs.as_str() == native_crs.as_str() {
        (coord.x, coord.y)
    } else {
        let degenerate = Bbox {
            minx: coord.x,
            miny: coord.y,
            maxx: coord.x,
            maxy: coord.y,
        };
        let transformed = transform_bbox(&degenerate, &crs, native_crs).with_context(|| {
            format!(
                "failed to transform sample coord from '{}' to '{}'",
                crs.as_str(),
                native_crs.as_str()
            )
        })?;
        (transformed.minx, transformed.miny)
    };

    // Step 2: pixel space.
    let gt = dataset
        .geo_transform()
        .context("dataset has no geotransform; cannot sample")?;
    let (ox, dx, oy, dy) = (gt[0], gt[1], gt[3], gt[5]);
    if gt[2] != 0.0 || gt[4] != 0.0 {
        return Err(anyhow!("rotated/sheared geotransform not supported for sample"));
    }

    let px = ((nx - ox) / dx).floor() as isize;
    let py = ((ny - oy) / dy).floor() as isize;

    let (raster_w, raster_h) = dataset.raster_size();
    if px < 0 || py < 0 || px as usize >= raster_w || py as usize >= raster_h {
        return Err(anyhow!(
            "sample coord ({}, {}) maps to pixel ({}, {}) which is outside the dataset ({} x {})",
            nx,
            ny,
            px,
            py,
            raster_w,
            raster_h
        ));
    }

    // Step 3: read one value per band, always as f64 for the output type.
    let bands = dataset.raster_count();
    let mut out = Vec::with_capacity(bands);
    for band_idx in 1..=bands {
        let band = dataset
            .rasterband(band_idx)
            .with_context(|| format!("failed to fetch band {band_idx} for sample"))?;
        let buf = band
            .read_as::<f64>(
                (px, py),
                (1, 1),
                (1, 1),
                Some(ResampleAlg::NearestNeighbour),
            )
            .with_context(|| format!("failed to read 1x1 pixel from band {band_idx}"))?;
        let v = buf
            .data()
            .first()
            .copied()
            .ok_or_else(|| anyhow!("empty 1x1 sample buffer for band {band_idx}"))?;
        out.push(PixelValue {
            band: band_idx as u32,
            value: v,
        });
    }

    Ok(out)
}
