//! How deep the durable cache tiers go per (layer, grid): `cache.max_level`.
//!
//! `auto`: the deepest level whose tiles are larger than one source file.
//! Such a tile opens several source files (a country-wide tile of the 2026
//! ortho opens all 498 remote COGs) and is slow to render on demand. A tile
//! smaller than a file opens one to four, each through that file's own
//! overviews: measured on the 2026 ortho (RD grid), the GDAL read per tile
//! falls from 1.5 s (level 3) to 92 ms (6), then flattens at 50-70 ms
//! (7-10) — the cost of reading the source GeoTIFF itself, so storing those
//! tiles buys little. Serving (`render_grid_tile`) and `kwms cache update`
//! both use this limit.

use anyhow::{anyhow, Context, Result};

use crate::config::{CacheMaxLevel, CacheMaxLevelMode, LayerConfig};
use crate::crs::{transform_bbox, Bbox};
use crate::source::gdal::footprint::{file_footprint, Footprint};
use crate::source::SourceMetadata;
use crate::tile::TileGrid;

/// The deepest durably cached level of `layer` in `grid` (`None`: no
/// level), with a human-readable reason.
pub fn layer_max_level(
    layer: &LayerConfig,
    grid: &dyn TileGrid,
    meta: &SourceMetadata,
) -> Result<(Option<u32>, String)> {
    let last = grid.level_count().saturating_sub(1);
    match layer.cache.max_level {
        CacheMaxLevel::Level(n) => Ok((Some(n.min(last)), format!("cache.max_level {n}"))),
        CacheMaxLevel::Mode(CacheMaxLevelMode::All) => Ok((Some(last), "cache.max_level all".into())),
        CacheMaxLevel::Mode(CacheMaxLevelMode::Auto) => {
            let source = layer
                .sources
                .first()
                .ok_or_else(|| anyhow!("layer '{}' has no sources", layer.name))?;
            let fp = file_footprint(source, meta).context("measuring the source files for cache.max_level auto")?;
            auto_max_level(grid, meta, &fp)
        }
    }
}

/// The deepest level whose tiles are larger (in either direction) than one
/// source file, measured at the centre of the source. `None` if even level
/// 0 tiles are smaller.
pub fn auto_max_level(grid: &dyn TileGrid, meta: &SourceMetadata, fp: &Footprint) -> Result<(Option<u32>, String)> {
    let b = meta.native_bbox;
    let (cx, cy) = ((b.minx + b.maxx) / 2.0, (b.miny + b.maxy) / 2.0);
    let file = Bbox {
        minx: cx - fp.width / 2.0,
        miny: cy - fp.height / 2.0,
        maxx: cx + fp.width / 2.0,
        maxy: cy + fp.height / 2.0,
    };
    let file = if meta.native_crs == *grid.crs() {
        file
    } else {
        transform_bbox(&file, &meta.native_crs, grid.crs()).context("source file size in the grid's CRS")?
    };
    let (fw, fh) = (file.maxx - file.minx, file.maxy - file.miny);
    let (tw, th) = grid.tile_size();
    let tile = |l: u32| (f64::from(tw) * grid.resolution(l), f64::from(th) * grid.resolution(l));
    let max = (0..grid.level_count())
        .rev()
        .find(|&l| tile(l).0 > fw || tile(l).1 > fh);
    let files = if fp.files == 1 {
        "the source is one file".to_string()
    } else {
        format!("{} source files", fp.files)
    };
    let reason = match max {
        Some(l) => format!(
            "auto: {files} of ~{:.0} x {:.0} grid units; level {l} tiles are {:.0}, level {} tiles {:.0}",
            fw,
            fh,
            tile(l).0,
            l.saturating_add(1),
            tile(l.saturating_add(1)).0
        ),
        None => format!("auto: {files}, every tile is smaller than a file"),
    };
    Ok((max, reason))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crs::Crs;
    use crate::source::PixelType;
    use crate::tile::grid::lookup_grid;

    fn meta_2026() -> SourceMetadata {
        SourceMetadata {
            native_crs: Crs::new("EPSG:28992"),
            native_bbox: Bbox {
                minx: 10_000.0,
                miny: 300_000.0,
                maxx: 290_000.0,
                maxy: 630_000.0,
            },
            band_count: 3,
            pixel_type: PixelType::U8,
            native_resolution: 0.08,
            nodata: None,
            size: (3_500_000, 4_125_000),
            overviews: vec![],
        }
    }

    /// The 2026 ortho (10 km COGs): RD tiles are 13.8 km at level 6 and
    /// 6.9 km at level 7, so cache 0-6.
    #[test]
    fn auto_level_for_10km_tiles_in_rd() {
        let grid = lookup_grid("rd_new_quad", &Default::default()).unwrap();
        let fp = Footprint {
            width: 10_000.0,
            height: 10_000.0,
            files: 498,
        };
        let (max, reason) = auto_max_level(grid.as_ref(), &meta_2026(), &fp).unwrap();
        assert_eq!(max, Some(6), "{reason}");
    }

    /// In Web Mercator a 10 km RD file is ~16 km of projected units at 52°N:
    /// level 11 tiles (19.6 km) are larger, level 12 (9.8 km) smaller.
    #[test]
    fn auto_level_reprojects_the_file_size() {
        let grid = lookup_grid("webmercator", &Default::default()).unwrap();
        let fp = Footprint {
            width: 10_000.0,
            height: 10_000.0,
            files: 498,
        };
        let (max, reason) = auto_max_level(grid.as_ref(), &meta_2026(), &fp).unwrap();
        assert_eq!(max, Some(11), "{reason}");
    }

    #[test]
    fn explicit_and_all_levels() {
        let grid = lookup_grid("rd_new_quad", &Default::default()).unwrap();
        let yaml = "name: l\ngrids: [rd_new_quad]\nsources: [{uri: x}]\ntitle: t\nformats: [png]\n";
        let mut layer: LayerConfig = serde_yaml::from_str(yaml).unwrap();
        layer.cache.max_level = CacheMaxLevel::Level(8);
        assert_eq!(layer_max_level(&layer, grid.as_ref(), &meta_2026()).unwrap().0, Some(8));
        layer.cache.max_level = CacheMaxLevel::Level(99);
        assert_eq!(
            layer_max_level(&layer, grid.as_ref(), &meta_2026()).unwrap().0,
            Some(grid.level_count() - 1)
        );
        layer.cache.max_level = CacheMaxLevel::Mode(CacheMaxLevelMode::All);
        assert_eq!(
            layer_max_level(&layer, grid.as_ref(), &meta_2026()).unwrap().0,
            Some(grid.level_count() - 1)
        );
        let auto: LayerConfig = serde_yaml::from_str(&format!("{yaml}cache: {{max_level: auto}}\n")).unwrap();
        assert_eq!(auto.cache.max_level, CacheMaxLevel::Mode(CacheMaxLevelMode::Auto));
    }
}
