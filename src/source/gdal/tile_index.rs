//! Probe a GTI tile index at startup, so a mosaic needs no hand-written
//! `open_options` and no fixed resolution.
//!
//! * **GSD per area.** The index (a shapefile, say) doesn't record the
//!   tiles' ground sample distance, and GTI otherwise takes it from the
//!   *first* tile and squeezes every other tile onto that grid with nearest
//!   neighbour. In a mixed mosaic (the 2026 ortho: 148 tiles at 5 cm, 350
//!   at 8 cm) that is blocky and loses detail. kwms opens every tile's
//!   header, groups the tiles by GSD and opens one GTI view per GSD
//!   ([`TileIndex::options_for`]); each read uses the view of the finest
//!   tiles under the requested window ([`TileIndex::finest_under`]). An
//!   8 cm area is read at 8 cm, a 5 cm area at 5 cm.
//! * **Resampling.** Where a window spans GSDs, the coarser tiles are
//!   resampled to the finer grid with `RESAMPLING=bilinear` instead of
//!   GTI's default nearest.
//! * **Overviews.** A mosaic has none. Virtual ones (`OVERVIEW_<n>_FACTOR`
//!   = 2, 4, …) until the whole mosaic fits a 4096 px read, so every level
//!   passes the startup read budget; GTI serves them from each tile's own
//!   overviews.
//!
//! An option set in `config.yaml` (`open_options`) or stored in the index
//! (layer metadata, e.g. a `.gti.gpkg`) is never overridden; setting
//! `RESX`/`RESY` there gives one fixed-resolution view as before.
//!
//! Tile headers are opened in parallel (remote COGs: one or two range
//! requests each). Credentials must be registered first
//! ([`super::credentials::register`]).
//!
//! One of the modules permitted to call into the `gdal` crate directly.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use gdal::vector::LayerAccess;
use gdal::{Dataset, DatasetOptions, GdalOpenFlags, Metadata};

use super::footprint::tile_index_path;

/// Tile headers opened at once.
const PROBE_THREADS: usize = 32;
/// Overview factors stop once the whole mosaic is at most this wide/high.
const COARSEST_OVERVIEW_PX: f64 = 4096.0;

/// What the tiles of an index look like.
#[derive(Debug, Clone)]
pub struct TileIndexInfo {
    pub tiles: usize,
    /// Tiles whose header couldn't be read (first few errors kept).
    pub failed: Vec<String>,
    pub failed_count: usize,
    /// Finest pixel size (x, y) over all tiles: the mosaic's GSD.
    pub finest: (f64, f64),
    /// Tile count per pixel size (x), e.g. {"0.05": 148, "0.08": 350}.
    pub resolutions: BTreeMap<String, usize>,
    /// Tiles without overviews (slow to read at coarse scales).
    pub without_overviews: usize,
    /// Index extent (minx, miny, maxx, maxy) in the index CRS.
    pub extent: (f64, f64, f64, f64),
    pub elapsed: Duration,
}

impl TileIndexInfo {
    pub fn mixed(&self) -> bool {
        self.resolutions.len() > 1
    }
}

/// What one tile header says.
struct Tile {
    res: (f64, f64),
    overviews: usize,
}

/// One tile of the index: its footprint (index CRS) and GSD.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct IndexTile {
    pub bbox: (f64, f64, f64, f64),
    /// Pixel size (x, y), rounded ([`round_res`]).
    pub res: (f64, f64),
}

/// A probed tile index: per-tile footprints and GSDs, and the GTI options
/// for each GSD.
#[derive(Debug, Clone)]
pub struct TileIndex {
    pub info: TileIndexInfo,
    /// Tiles whose header could be read.
    pub tiles: Vec<IndexTile>,
    /// Their footprint polygons (WKB, index CRS): where the mosaic has data.
    pub footprints: Vec<Vec<u8>>,
    configured: Vec<String>,
    stored: Vec<String>,
}

impl TileIndex {
    /// Whether the config or the index fixes the resolution itself (then
    /// one view, as configured).
    pub fn resolution_fixed(&self) -> bool {
        is_set(&self.configured, &self.stored, "RESX") || is_set(&self.configured, &self.stored, "RESY")
    }

    /// Distinct GSDs, finest first.
    pub fn resolutions(&self) -> Vec<(f64, f64)> {
        let mut out: Vec<(f64, f64)> = Vec::new();
        for t in &self.tiles {
            if !out.contains(&t.res) {
                out.push(t.res);
            }
        }
        out.sort_by(|a, b| a.0.total_cmp(&b.0));
        out
    }

    /// GTI open options for a view at GSD `res`.
    pub fn options_for(&self, res: (f64, f64)) -> Vec<String> {
        derive_options(&self.info, res, &self.configured, &self.stored)
    }

    /// The finest GSD among `tiles` (footprints in the request's CRS) that
    /// intersect `bbox`; `None` if none does.
    pub fn finest_under(tiles: &[IndexTile], bbox: (f64, f64, f64, f64)) -> Option<(f64, f64)> {
        let (minx, miny, maxx, maxy) = bbox;
        tiles
            .iter()
            .filter(|t| t.bbox.0 < maxx && minx < t.bbox.2 && t.bbox.1 < maxy && miny < t.bbox.3)
            .map(|t| t.res)
            .min_by(|a, b| a.0.total_cmp(&b.0))
    }
}

/// Probe `uri` if it is a GTI tile index (`None` otherwise).
pub fn probe(uri: &str, open_options: &[String]) -> Result<Option<TileIndex>> {
    let Some(index) = tile_index_path(uri) else {
        return Ok(None);
    };
    let started = Instant::now();
    let (entries, extent, stored) =
        read_index(index, open_options).with_context(|| format!("reading tile index '{index}'"))?;
    let locations: Vec<&str> = entries.iter().map(|(l, _, _)| l.as_str()).collect();
    if locations.is_empty() {
        bail!("tile index '{index}' has no tiles");
    }

    // Open every tile header, PROBE_THREADS at a time; each thread owns the
    // datasets it opens (GDAL handles are never shared between threads).
    let next = AtomicUsize::new(0);
    let results: Mutex<Vec<(usize, Result<Tile, String>)>> = Mutex::new(Vec::with_capacity(locations.len()));
    std::thread::scope(|scope| {
        for _ in 0..PROBE_THREADS.min(locations.len()) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(location) = locations.get(i) else { break };
                let tile = read_tile(location).map_err(|e| format!("{location}: {e:#}"));
                if let Ok(mut r) = results.lock() {
                    r.push((i, tile));
                }
            });
        }
    });
    let mut results = results
        .into_inner()
        .map_err(|_| anyhow!("tile probe thread panicked"))?;
    results.sort_by_key(|(i, _)| *i);
    let mut tiles = Vec::with_capacity(results.len());
    let mut footprints = Vec::with_capacity(results.len());

    let mut info = TileIndexInfo {
        tiles: locations.len(),
        failed: Vec::new(),
        failed_count: 0,
        finest: (f64::INFINITY, f64::INFINITY),
        resolutions: BTreeMap::new(),
        without_overviews: 0,
        extent,
        elapsed: Duration::ZERO,
    };
    for (i, r) in results {
        match r {
            Ok(t) => {
                let res = (round_res(t.res.0), round_res(t.res.1));
                if let Some((_, bbox, wkb)) = entries.get(i) {
                    tiles.push(IndexTile { bbox: *bbox, res });
                    footprints.push(wkb.clone());
                }
                info.finest = (info.finest.0.min(t.res.0), info.finest.1.min(t.res.1));
                let n = info.resolutions.entry(format!("{}", round_res(t.res.0))).or_default();
                *n = n.saturating_add(1);
                if t.overviews == 0 {
                    info.without_overviews = info.without_overviews.saturating_add(1);
                }
            }
            Err(e) => {
                info.failed_count = info.failed_count.saturating_add(1);
                if info.failed.len() < 5 {
                    info.failed.push(e);
                }
            }
        }
    }
    if !(info.finest.0.is_finite() && info.finest.1.is_finite()) {
        bail!(
            "none of the {} tiles of '{index}' could be opened (credentials?); first errors: {:?}",
            info.tiles,
            info.failed
        );
    }
    info.elapsed = started.elapsed();
    Ok(Some(TileIndex {
        info,
        tiles,
        footprints,
        configured: open_options.to_vec(),
        stored,
    }))
}

/// Locations of every tile (resolved like GTI does: relative to the index),
/// the index extent, and the GTI option names stored in the index itself.
type Bounds = (f64, f64, f64, f64);
/// Tile locations with footprints, the index extent, and GTI option names
/// stored in the index.
type IndexContents = (Vec<(String, Bounds, Vec<u8>)>, Bounds, Vec<String>);

fn read_index(index: &str, open_options: &[String]) -> Result<IndexContents> {
    let options = DatasetOptions {
        open_flags: GdalOpenFlags::GDAL_OF_VECTOR | GdalOpenFlags::GDAL_OF_READONLY,
        ..Default::default()
    };
    let ds = Dataset::open_ex(Path::new(index), options).context("opening the index as a vector dataset")?;
    let option = |key: &str| {
        open_options
            .iter()
            .find_map(|o| o.strip_prefix(key)?.strip_prefix('=').map(str::to_string))
    };
    let mut layer = match option("LAYER") {
        Some(name) => ds
            .layer_by_name(&name)
            .with_context(|| format!("index has no layer '{name}'"))?,
        None if ds.layer_count() == 1 => ds.layer(0)?,
        None => bail!("index has {} layers; set the LAYER open option", ds.layer_count()),
    };
    let stored: Vec<String> = ["RESX", "RESY", "RESAMPLING", "OVERVIEW_0_FACTOR", "OVERVIEW_1_FACTOR"]
        .into_iter()
        .filter(|k| layer.metadata_item(k, "").is_some())
        .map(String::from)
        .collect();
    let field = option("LOCATION_FIELD")
        .or_else(|| layer.metadata_item("LOCATION_FIELD", ""))
        .unwrap_or_else(|| "location".into());
    let dir = Path::new(index)
        .parent()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut locations = Vec::new();
    for feature in layer.features() {
        let Some(loc) = feature
            .field_as_string_by_name(&field)
            .with_context(|| format!("reading field '{field}'"))?
        else {
            continue;
        };
        let Some(geometry) = feature.geometry() else { continue };
        let e = geometry.envelope();
        // GTI resolves relative locations against the index's directory.
        let absolute = loc.starts_with('/') || loc.contains(':');
        let loc = if absolute || dir.is_empty() {
            loc
        } else {
            format!("{dir}/{loc}")
        };
        locations.push((loc, (e.MinX, e.MinY, e.MaxX, e.MaxY), geometry.wkb()?));
    }
    let e = layer.get_extent().context("index extent")?;
    Ok((locations, (e.MinX, e.MinY, e.MaxX, e.MaxY), stored))
}

fn read_tile(location: &str) -> Result<Tile> {
    let options = DatasetOptions {
        open_flags: GdalOpenFlags::GDAL_OF_RASTER | GdalOpenFlags::GDAL_OF_READONLY,
        ..Default::default()
    };
    let ds = Dataset::open_ex(Path::new(location), options)?;
    let gt = ds.geo_transform().context("no geotransform")?;
    let res = (gt[1].hypot(gt[2]), gt[4].hypot(gt[5]));
    if !(res.0 > 0.0 && res.1 > 0.0) {
        bail!("degenerate pixel size {res:?}");
    }
    let overviews = usize::try_from(ds.rasterband(1)?.overview_count()?).unwrap_or(0);
    Ok(Tile { res, overviews })
}

/// Pixel sizes rounded to 0.1 mm-ish, so float noise doesn't split groups.
fn round_res(r: f64) -> f64 {
    let scale = 10f64.powi(6);
    (r * scale).round() / scale
}

fn is_set(configured: &[String], stored: &[String], key: &str) -> bool {
    configured
        .iter()
        .any(|o| o.split_once('=').is_some_and(|(k, _)| k.eq_ignore_ascii_case(key)))
        || stored.iter().any(|s| s.eq_ignore_ascii_case(key))
}

/// GTI open options for a view at GSD `res`, minus any the config or the
/// index already sets.
fn derive_options(info: &TileIndexInfo, res: (f64, f64), configured: &[String], stored: &[String]) -> Vec<String> {
    let set = |key: &str| is_set(configured, stored, key);
    let mut out = Vec::new();
    let (rx, ry) = res;
    let derive_res = !set("RESX") && !set("RESY");
    if derive_res {
        out.push(format!("RESX={rx}"));
        out.push(format!("RESY={ry}"));
    }
    let resampling = (info.mixed() && !set("RESAMPLING")).then_some("RESAMPLING=bilinear");
    out.extend(resampling.map(String::from));
    let any_overview = |list: &[String]| list.iter().any(|o| o.to_ascii_uppercase().starts_with("OVERVIEW_"));
    if any_overview(configured) || any_overview(stored) {
        return out;
    }

    let (minx, miny, maxx, maxy) = info.extent;
    // Full-resolution size in pixels (an extent that is a whole number of
    // pixels must not gain one from float noise).
    let pixels = |span: f64, r: f64| ((span / r) - 1e-9).ceil().max(1.0);
    let (w, h) = (pixels(maxx - minx, rx), pixels(maxy - miny, ry));
    // Powers of two until the whole mosaic fits COARSEST_OVERVIEW_PX
    // (capped: 2^30 covers any raster GDAL can address).
    let mut factors: Vec<u64> = Vec::new();
    let mut factor: u64 = 2;
    while factors.len() < 30 {
        factors.push(factor);
        if w.max(h) / factor as f64 <= COARSEST_OVERVIEW_PX {
            break;
        }
        factor = factor.saturating_mul(2);
    }
    let largest = factors.last().copied().unwrap_or(2) as f64;

    // GDAL maps an overview's pixels by the size ratio full/overview. If the
    // mosaic isn't a whole number of overview pixels, GTI rounds the
    // overview size up and the overview's pixels come out slightly too
    // small: the image drifts away from the top-left corner (measured on the
    // 2026 ortho: up to half an overview pixel by Rotterdam, a -0.3 px bias
    // in Web Mercator tiles). So pad the extent (top-left fixed) to a whole
    // number of the largest overview's pixels, and give every overview the
    // same resolution and extent: GTI opens overviews from the index with
    // only their own `OVERVIEW_<n>_OPEN_OPTIONS`, not the parent's options.
    let padded = derive_res.then(|| {
        let (pw, ph) = ((w / largest).ceil() * largest, (h / largest).ceil() * largest);
        [
            format!("MINX={minx}"),
            format!("MINY={}", maxy - ph * ry),
            format!("MAXX={}", minx + pw * rx),
            format!("MAXY={maxy}"),
        ]
    });
    if let Some(extent) = &padded {
        out.extend(extent.iter().cloned());
    }
    for (n, f) in factors.iter().enumerate() {
        out.push(format!("OVERVIEW_{n}_FACTOR={f}"));
        if let Some(extent) = &padded {
            let mut own = vec![format!("RESX={rx}"), format!("RESY={ry}")];
            own.extend(extent.iter().cloned());
            own.extend(resampling.map(String::from));
            out.push(format!("OVERVIEW_{n}_OPEN_OPTIONS={}", own.join(",")));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(resolutions: &[(f64, usize)], extent_m: (f64, f64)) -> TileIndexInfo {
        let finest = resolutions.iter().map(|r| r.0).fold(f64::INFINITY, f64::min);
        TileIndexInfo {
            tiles: resolutions.iter().map(|r| r.1).sum(),
            failed: vec![],
            failed_count: 0,
            finest: (finest, finest),
            resolutions: resolutions.iter().map(|(r, n)| (format!("{r}"), *n)).collect(),
            without_overviews: 0,
            extent: (0.0, 0.0, extent_m.0, extent_m.1),
            elapsed: Duration::ZERO,
        }
    }

    /// The 2026 ortho: 5 cm wins, mixed -> bilinear, overviews to 2048
    /// (6.6M px / 2048 = 3223 <= 4096).
    #[test]
    fn mixed_mosaic_gets_finest_gsd_and_enough_overviews() {
        let i = info(&[(0.05, 148), (0.08, 350)], (280_000.0, 330_000.0));
        let opts = derive_options(&i, (0.05, 0.05), &[], &[]);
        assert_eq!(&opts[..3], ["RESX=0.05", "RESY=0.05", "RESAMPLING=bilinear"]);
        assert!(opts.contains(&"OVERVIEW_10_FACTOR=2048".to_string()), "{opts:?}");
        assert!(!opts.iter().any(|o| o.starts_with("OVERVIEW_11_")));
        // The 8 cm view: 4.1M px / 1024 = 4029 <= 4096.
        let opts = derive_options(&i, (0.08, 0.08), &[], &[]);
        assert_eq!(&opts[..2], ["RESX=0.08", "RESY=0.08"]);
        assert!(opts.contains(&"OVERVIEW_9_FACTOR=1024".to_string()), "{opts:?}");
        assert!(!opts.iter().any(|o| o.starts_with("OVERVIEW_10_")));
    }

    /// The extent is padded to whole pixels of the largest overview
    /// (top-left fixed), and every overview gets the same resolution and
    /// extent — otherwise GDAL's size-ratio mapping drifts the image.
    #[test]
    fn extent_is_padded_for_exact_overviews() {
        // The 2026 index: 280 x 330 km at 8 cm = 3500000 x 4125000 px.
        let opts = derive_options(&info(&[(0.08, 498)], (280_000.0, 330_000.0)), (0.08, 0.08), &[], &[]);
        let get = |k: &str| {
            opts.iter()
                .find_map(|o| o.strip_prefix(&format!("{k}=")))
                .unwrap()
                .parse::<f64>()
                .unwrap()
        };
        let (minx, miny, maxx, maxy) = (get("MINX"), get("MINY"), get("MAXX"), get("MAXY"));
        assert_eq!((minx, maxy), (0.0, 330_000.0), "top-left fixed");
        let (w, h) = (((maxx - minx) / 0.08).round(), ((maxy - miny) / 0.08).round());
        assert_eq!((w % 1024.0, h % 1024.0), (0.0, 0.0), "{w} x {h}");
        assert!(w >= 3_500_000.0 && h >= 4_125_000.0 && w - 3_500_000.0 < 1024.0 && h - 4_125_000.0 < 1024.0);
        let own = opts.iter().find(|o| o.starts_with("OVERVIEW_0_OPEN_OPTIONS=")).unwrap();
        assert!(own.contains("RESX=0.08,RESY=0.08,MINX=0,"), "{own}");
        assert_eq!(opts.iter().filter(|o| o.contains("_OPEN_OPTIONS=")).count(), 10);
    }

    #[test]
    fn single_resolution_needs_no_resampling() {
        let opts = derive_options(&info(&[(0.08, 498)], (280_000.0, 330_000.0)), (0.08, 0.08), &[], &[]);
        assert!(opts.contains(&"RESX=0.08".to_string()));
        assert!(!opts.iter().any(|o| o.contains("RESAMPLING")));
        assert!(opts.contains(&"OVERVIEW_9_FACTOR=1024".to_string()));
    }

    #[test]
    fn finest_under_picks_the_finest_intersecting_tile() {
        let tiles = [
            IndexTile {
                bbox: (0.0, 0.0, 10.0, 10.0),
                res: (0.08, 0.08),
            },
            IndexTile {
                bbox: (10.0, 0.0, 20.0, 10.0),
                res: (0.05, 0.05),
            },
        ];
        assert_eq!(
            TileIndex::finest_under(&tiles, (1.0, 1.0, 2.0, 2.0)),
            Some((0.08, 0.08))
        );
        assert_eq!(
            TileIndex::finest_under(&tiles, (9.0, 1.0, 11.0, 2.0)),
            Some((0.05, 0.05))
        );
        assert_eq!(TileIndex::finest_under(&tiles, (30.0, 30.0, 31.0, 31.0)), None);
    }

    #[test]
    fn configured_or_stored_options_win() {
        let i = info(&[(0.05, 1), (0.08, 1)], (10_000.0, 10_000.0));
        let opts = derive_options(
            &i,
            (0.05, 0.05),
            &["RESX=0.1".into(), "OVERVIEW_0_FACTOR=4".into()],
            &["RESAMPLING".into()],
        );
        assert!(opts.is_empty(), "{opts:?}");
    }

    /// The shipped 2026 index, read locally (no tile opens).
    #[test]
    fn reads_the_shipped_index() {
        let index = concat!(env!("CARGO_MANIFEST_DIR"), "/data/2026_orthoHR/tiles.shp");
        if !Path::new(index).exists() {
            return;
        }
        let (entries, extent, stored) = read_index(index, &[]).unwrap();
        assert_eq!(entries.len(), 498);
        assert!(
            entries[0].0.starts_with("/vsiaz/open-tiles/2026hrrgb/"),
            "{}",
            entries[0].0
        );
        let (minx, miny, maxx, maxy) = entries[0].1;
        assert!(!entries[0].2.is_empty(), "footprint WKB");
        assert_eq!((maxx - minx, maxy - miny), (10_000.0, 10_000.0));
        assert_eq!(extent, (10_000.0, 300_000.0, 290_000.0, 630_000.0));
        assert!(stored.is_empty());
    }
}
