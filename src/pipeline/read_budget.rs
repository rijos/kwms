//! Startup check: every tile a layer can serve must be renderable from its
//! source by reading at most [`MAX_SOURCE_READ_PIXELS`] source pixels —
//! "overviews or bail".
//!
//! A coarse tile covers a large part of the source. GDALWarp then reads
//! from the coarsest overview whose decimation suits the tile (`-ovr AUTO`)
//! — and without a suitable overview, from the full-resolution raster. One
//! z0 tile from a 40000×40000 source without overviews took 12 s and 7 GB
//! of RAM; with overviews, 0.08 s. Tile coordinates are client-chosen, so
//! every level of every grid a layer is served in must be cheap. A source
//! without the overviews for that is a configuration error, reported at
//! startup — never a slow tile at request time.
//!
//! The estimate mirrors GDAL 3.11's selection (`gdalwarp_lib.cpp:2785-2910`):
//! the target ratio is the smaller of the x/y source-pixels-per-output-pixel
//! ratios, and the chosen overview is the coarsest whose x decimation is
//! below `ratio + 0.1`.

use anyhow::{bail, Context, Result};

use crate::crs::{transform_bbox, Bbox, Crs};
use crate::source::SourceMetadata;
use crate::tile::TileGrid;

/// Most source pixels (per band) one tile may read: 4096² ≈ 16.8 M. With
/// power-of-two overviews a tile reads at most ~(2 × tile size)² pixels, so
/// this only trips when overviews are missing or far too sparse. At the
/// measured ~130 M pixels/s it bounds a tile's read to roughly 0.1 s.
pub const MAX_SOURCE_READ_PIXELS: f64 = 4096.0 * 4096.0;

/// GDAL's overview-selection tolerance (`EPSILON` in `gdalwarp_lib.cpp`).
const GDAL_OVR_EPSILON: f64 = 0.1;

/// Check every level of `grid` for source `uri`, whose data covers
/// `coverage` (in the grid's CRS). Errors name the source, grid and level,
/// and the overview decimation that would be needed.
pub fn check(uri: &str, meta: &SourceMetadata, grid: &dyn TileGrid, coverage: &Bbox) -> Result<()> {
    for level in 0..grid.level_count() {
        let read = pixels_read(meta, grid, level, coverage)
            .with_context(|| format!("estimating source reads of `{uri}` for grid `{}` level {level}", grid.name()))?;
        let Some(read) = read else { continue };
        if read.pixels > MAX_SOURCE_READ_PIXELS {
            let needed = (read.full_res_pixels / MAX_SOURCE_READ_PIXELS).sqrt().ceil();
            bail!(
                "source `{uri}` lacks overviews: a tile of grid `{}` at level {level} would read about {:.0} \
                 source pixels (limit {MAX_SOURCE_READ_PIXELS:.0}); it needs an overview decimated at least \
                 {needed:.0}x (e.g. `gdaladdo -r average <file> 2 4 8 16 …`)",
                grid.name(),
                read.pixels,
            );
        }
    }
    Ok(())
}

/// What one tile at a level reads from the source.
#[derive(Debug)]
struct Read {
    /// Pixels read from the overview GDAL would pick.
    pixels: f64,
    /// The same footprint at full resolution.
    full_res_pixels: f64,
}

/// Source pixels read by the worst tile at `level`, or `None` if no tile at
/// that level touches the source.
///
/// The worst tile is the one overlapping the data most: a tile-sized window
/// centred on the coverage, clipped to it (no aligned tile overlaps more).
fn pixels_read(meta: &SourceMetadata, grid: &dyn TileGrid, level: u32, coverage: &Bbox) -> Result<Option<Read>> {
    let (tile_w, tile_h) = grid.tile_size();
    let res = grid.resolution(level);
    let (half_w, half_h) = (f64::from(tile_w) * res / 2.0, f64::from(tile_h) * res / 2.0);
    let (cx, cy) = ((coverage.minx + coverage.maxx) / 2.0, (coverage.miny + coverage.maxy) / 2.0);
    let window = clip(&Bbox { minx: cx - half_w, miny: cy - half_h, maxx: cx + half_w, maxy: cy + half_h }, coverage);
    let Some(window) = window else { return Ok(None) };
    // Output pixels the window spans (at most one tile).
    let out_w = ((window.maxx - window.minx) / res).max(1.0);
    let out_h = ((window.maxy - window.miny) / res).max(1.0);

    // Its footprint in the source, in full-resolution source pixels.
    let footprint = if same_token(grid.crs(), &meta.native_crs) {
        window
    } else {
        transform_bbox(&window, grid.crs(), &meta.native_crs)?
    };
    let Some(footprint) = clip(&footprint, &meta.native_bbox) else { return Ok(None) };
    let (src_w, src_h) = (f64::from(meta.size.0), f64::from(meta.size.1));
    let nb = &meta.native_bbox;
    let fx = (footprint.maxx - footprint.minx) / (nb.maxx - nb.minx) * src_w;
    let fy = (footprint.maxy - footprint.miny) / (nb.maxy - nb.miny) * src_h;

    // GDAL's -ovr AUTO choice for this ratio.
    let ratio = (fx / out_w).min(fy / out_h);
    let (mut kx, mut ky) = (1.0, 1.0);
    if ratio > 1.0 {
        for &(ow, oh) in &meta.overviews {
            let ox = src_w / f64::from(ow.max(1));
            if ox < ratio + GDAL_OVR_EPSILON && ox > kx {
                (kx, ky) = (ox, src_h / f64::from(oh.max(1)));
            }
        }
    }
    let pixels = (fx / kx).ceil() * (fy / ky).ceil();
    if !pixels.is_finite() {
        bail!("non-finite source footprint {footprint:?}");
    }
    Ok(Some(Read { pixels, full_res_pixels: fx * fy }))
}

fn clip(a: &Bbox, b: &Bbox) -> Option<Bbox> {
    let c = Bbox { minx: a.minx.max(b.minx), miny: a.miny.max(b.miny), maxx: a.maxx.min(b.maxx), maxy: a.maxy.min(b.maxy) };
    (c.minx < c.maxx && c.miny < c.maxy).then_some(c)
}

/// Same CRS token: no transform needed. (Different tokens for the same CRS
/// just take the GDAL transform, which is then the identity.)
fn same_token(a: &Crs, b: &Crs) -> bool {
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::PixelType;
    use crate::tile::grid::{RdNewQuad, WebMercator};

    /// The 40000² RD New source from the investigation (12 s / 7 GB per z0
    /// tile without overviews), as metadata.
    fn big_rd(overviews: Vec<(u32, u32)>) -> SourceMetadata {
        SourceMetadata {
            native_crs: Crs::new("EPSG:28992"),
            native_bbox: Bbox::from([0.0, 300_000.0, 280_000.0, 625_000.0]),
            band_count: 3,
            pixel_type: PixelType::U8,
            native_resolution: 7.0,
            nodata: None,
            size: (40_000, 40_000),
            overviews,
        }
    }

    fn pyramid(size: u32, factors: &[u32]) -> Vec<(u32, u32)> {
        factors.iter().map(|f| (size.div_ceil(*f), size.div_ceil(*f))).collect()
    }

    #[test]
    fn large_source_without_overviews_bails_in_its_own_and_other_crses() {
        let meta = big_rd(Vec::new());
        let rd = RdNewQuad::new();
        let err = check("big.tif", &meta, &rd, &meta.native_bbox).unwrap_err().to_string();
        assert!(err.contains("lacks overviews") && err.contains("level 0"), "{err}");
        let wm = WebMercator::new();
        let cov = transform_bbox(&meta.native_bbox, &meta.native_crs, wm.crs()).unwrap();
        assert!(check("big.tif", &meta, &wm, &cov).is_err());
    }

    #[test]
    fn large_source_with_a_full_pyramid_passes() {
        let meta = big_rd(pyramid(40_000, &[2, 4, 8, 16, 32, 64, 128, 256]));
        check("big.tif", &meta, &RdNewQuad::new(), &meta.native_bbox).unwrap();
        let wm = WebMercator::new();
        let cov = transform_bbox(&meta.native_bbox, &meta.native_crs, wm.crs()).unwrap();
        check("big.tif", &meta, &wm, &cov).unwrap();
    }

    #[test]
    fn a_pyramid_that_stops_too_early_bails() {
        // Overviews only down to 1/4: coarse tiles still read ~10^8 pixels.
        let meta = big_rd(pyramid(40_000, &[2, 4]));
        assert!(check("big.tif", &meta, &RdNewQuad::new(), &meta.native_bbox).is_err());
    }

    #[test]
    fn small_sources_need_no_overviews() {
        let mut meta = big_rd(Vec::new());
        meta.size = (4000, 4000);
        check("small.tif", &meta, &RdNewQuad::new(), &meta.native_bbox).unwrap();
    }

    #[test]
    fn estimate_matches_gdal_overview_choice() {
        // RD level 0 is one 256 px tile of 880 km (3440.64 m/px). The source
        // spans 280×325 km of it: ~81×94 output px reading 40000² source px,
        // ratios 491 / 423 → target 423. GDAL takes the coarsest overview
        // decimated below 423.1: 1/256 (157 px), not 1/512 (79 px ≈ 506×).
        let meta = big_rd(pyramid(40_000, &[2, 4, 8, 16, 32, 64, 128, 256, 512]));
        let read = pixels_read(&meta, &RdNewQuad::new(), 0, &meta.native_bbox).unwrap().unwrap();
        assert!((157.0 * 157.0..=158.0 * 158.0).contains(&read.pixels), "{read:?}");
        assert_eq!(read.full_res_pixels, 40_000.0 * 40_000.0);
    }
}
