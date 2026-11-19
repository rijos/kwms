//! How large one source *file* is on the ground — the unit of cost when
//! rendering from a mosaic.
//!
//! A tile that is larger than one source file opens several (for the 2026
//! ortho: up to all 498 remote COGs for a country-wide tile); a tile smaller
//! than a file opens one to four, each through that file's own overviews —
//! about as cheap as it gets, so caching it buys little. `kwms cache update
//! --max-level auto` seeds exactly the levels whose tiles are larger than a
//! file.
//!
//! One of the modules permitted to call into the `gdal` crate directly.

use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use gdal::vector::LayerAccess;
use gdal::{Dataset, DatasetOptions, GdalOpenFlags};

use crate::config::SourceConfig;
use crate::crs::{same_crs, Crs};
use crate::source::SourceMetadata;

/// Typical ground size of one source file, in the source's native CRS units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Footprint {
    pub width: f64,
    pub height: f64,
    /// Number of files it was measured over (1 for a single dataset).
    pub files: usize,
}

/// For a GTI tile index (`GTI:<index>`, `*.gti.gpkg`, `*.gti.fgb`): the
/// median width and height of its tiles. For anything else: the whole
/// dataset's extent (a VRT mosaic is treated as one file — pass an explicit
/// level for those).
pub fn file_footprint(source: &SourceConfig, meta: &SourceMetadata) -> Result<Footprint> {
    let Some(index) = tile_index_path(&source.uri) else {
        let b = meta.native_bbox;
        return Ok(Footprint {
            width: b.maxx - b.minx,
            height: b.maxy - b.miny,
            files: 1,
        });
    };
    index_footprint(index, source, &meta.native_crs).with_context(|| format!("measuring the tiles of index '{index}'"))
}

/// The vector index behind a GTI URI, if the URI is one.
pub(crate) fn tile_index_path(uri: &str) -> Option<&str> {
    if let Some(rest) = uri
        .get(..4)
        .filter(|p| p.eq_ignore_ascii_case("GTI:"))
        .map(|_| &uri[4..])
    {
        return Some(rest);
    }
    let lower = uri.to_ascii_lowercase();
    (lower.ends_with(".gti.gpkg") || lower.ends_with(".gti.fgb")).then_some(uri)
}

fn index_footprint(index: &str, source: &SourceConfig, native: &Crs) -> Result<Footprint> {
    let options = DatasetOptions {
        open_flags: GdalOpenFlags::GDAL_OF_VECTOR | GdalOpenFlags::GDAL_OF_READONLY,
        ..Default::default()
    };
    let ds = Dataset::open_ex(Path::new(index), options).context("opening the index as a vector dataset")?;
    // Same layer choice as GTI: the LAYER open option, else the only layer.
    let wanted = source.open_options.iter().find_map(|o| o.strip_prefix("LAYER="));
    let mut layer = match wanted {
        Some(name) => ds
            .layer_by_name(name)
            .with_context(|| format!("index has no layer '{name}'"))?,
        None if ds.layer_count() == 1 => ds.layer(0)?,
        None => bail!("index has {} layers; set the LAYER open option", ds.layer_count()),
    };
    if let Some(srs) = layer.spatial_ref() {
        let index_crs = Crs::new(srs.to_wkt()?);
        if !same_crs(&index_crs, native)? {
            bail!("the index is not in the mosaic's CRS ({native}); pass an explicit --max-level");
        }
    }

    let (mut widths, mut heights) = (Vec::new(), Vec::new());
    for feature in layer.features() {
        if let Some(g) = feature.geometry() {
            let e = g.envelope();
            let (w, h) = (e.MaxX - e.MinX, e.MaxY - e.MinY);
            if w.is_finite() && h.is_finite() && w > 0.0 && h > 0.0 {
                widths.push(w);
                heights.push(h);
            }
        }
    }
    let files = widths.len();
    let width = median(&mut widths).ok_or_else(|| anyhow!("index has no tiles with an extent"))?;
    let height = median(&mut heights).ok_or_else(|| anyhow!("index has no tiles with an extent"))?;
    Ok(Footprint { width, height, files })
}

fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    Some(v[v.len() / 2])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_tile_index_uris() {
        assert_eq!(tile_index_path("GTI:data/tiles.shp"), Some("data/tiles.shp"));
        assert_eq!(tile_index_path("gti:/vsiaz/c/tiles.shp"), Some("/vsiaz/c/tiles.shp"));
        assert_eq!(tile_index_path("data/ortho.gti.gpkg"), Some("data/ortho.gti.gpkg"));
        assert_eq!(tile_index_path("data/ortho.vrt"), None);
        assert_eq!(tile_index_path("/vsiaz/c/x.tif"), None);
    }

    /// The shipped 2026 index: 498 tiles of 10 x 10 km.
    #[test]
    fn measures_the_shipped_2026_index() {
        let index = concat!(env!("CARGO_MANIFEST_DIR"), "/data/2026_orthoHR/tiles.shp");
        if !Path::new(index).exists() {
            return;
        }
        let src = SourceConfig {
            uri: format!("GTI:{index}"),
            ..Default::default()
        };
        let fp = index_footprint(index, &src, &Crs::new("EPSG:28992")).unwrap();
        assert_eq!(
            fp,
            Footprint {
                width: 10_000.0,
                height: 10_000.0,
                files: 498
            }
        );
    }

    #[test]
    fn median_picks_the_middle() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0]), Some(2.0));
        assert_eq!(median(&mut []), None);
    }
}
