//! `TileGrid` trait and grid implementations.

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{GridConfig, GridOrigin};
use crate::crs::{Bbox, Crs};

use super::TileCoord;

/// Where tile (col=0, row=0) lives in the grid's CRS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    TopLeft,
    BottomLeft,
}

/// A tile grid — CRS + origin + tile size + resolutions ladder.
///
/// Well-known grids (`webmercator`, `global_geodetic`, `rd_new_quad`, …) are
/// built-in implementations of this trait. Custom grids declared in
/// `config.yaml` have their own implementation backed by `GridConfig`.
pub trait TileGrid: Send + Sync {
    /// Stable identifier (e.g. `"webmercator"`).
    fn name(&self) -> &str;
    /// CRS the grid is defined in.
    fn crs(&self) -> &Crs;
    /// Origin convention.
    fn origin(&self) -> Origin;
    /// (width, height) of a tile in pixels.
    fn tile_size(&self) -> (u32, u32);
    /// CRS-units-per-pixel at the given level.
    fn resolution(&self, level: u32) -> f64;
    /// Number of advertised levels.
    fn level_count(&self) -> u32;
    /// Bbox of a specific tile.
    fn tile_bbox(&self, coord: TileCoord) -> Bbox;
}

// ---------------------------------------------------------------------------
// Quad-tree grid helper
// ---------------------------------------------------------------------------

/// Internal helper: a power-of-two quad-tree grid with a TopLeft origin.
///
/// Built-in WMTS-style grids (WebMercator, GoogleCRS84Quad, RDNewQuad, …) are
/// all instances of this shape — they differ only in CRS, top-left anchor,
/// base resolution, tile size, level count, and (for GoogleCRS84Quad) the
/// number of tiles spanning the world at level 0.
struct QuadGrid {
    name: &'static str,
    crs: Crs,
    tile_size: (u32, u32),
    /// `(x, y)` of the (col=0, row=0) tile corner in the grid's CRS.
    top_left: (f64, f64),
    /// CRS-units-per-pixel at level 0.
    base_resolution: f64,
    level_count: u32,
}

impl QuadGrid {
    fn resolution(&self, level: u32) -> f64 {
        self.base_resolution / (1u64 << level) as f64
    }

    fn tile_bbox(&self, coord: TileCoord) -> Bbox {
        let res = self.resolution(coord.z);
        let tile_w = self.tile_size.0 as f64 * res;
        let tile_h = self.tile_size.1 as f64 * res;
        let minx = self.top_left.0 + coord.x as f64 * tile_w;
        let maxx = minx + tile_w;
        let maxy = self.top_left.1 - coord.y as f64 * tile_h;
        let miny = maxy - tile_h;
        Bbox { minx, miny, maxx, maxy }
    }
}

// ---------------------------------------------------------------------------
// WebMercator — OGC:1.0:GoogleMapsCompatible (EPSG:3857)
// ---------------------------------------------------------------------------

/// Half-extent of the WebMercator world in meters.
const WEBMERCATOR_HALF_EXTENT: f64 = 20037508.3427892;

pub struct WebMercator {
    inner: QuadGrid,
}

impl WebMercator {
    pub fn new() -> Self {
        Self {
            inner: QuadGrid {
                name: "webmercator",
                crs: Crs::new("EPSG:3857"),
                tile_size: (256, 256),
                top_left: (-WEBMERCATOR_HALF_EXTENT, WEBMERCATOR_HALF_EXTENT),
                base_resolution: 2.0 * WEBMERCATOR_HALF_EXTENT / 256.0,
                level_count: 22,
            },
        }
    }
}

impl Default for WebMercator {
    fn default() -> Self {
        Self::new()
    }
}

impl TileGrid for WebMercator {
    fn name(&self) -> &str {
        self.inner.name
    }
    fn crs(&self) -> &Crs {
        &self.inner.crs
    }
    fn origin(&self) -> Origin {
        Origin::TopLeft
    }
    fn tile_size(&self) -> (u32, u32) {
        self.inner.tile_size
    }
    fn resolution(&self, level: u32) -> f64 {
        self.inner.resolution(level)
    }
    fn level_count(&self) -> u32 {
        self.inner.level_count
    }
    fn tile_bbox(&self, coord: TileCoord) -> Bbox {
        self.inner.tile_bbox(coord)
    }
}

// ---------------------------------------------------------------------------
// GlobalGeodetic — WMTS GoogleCRS84Quad (EPSG:4326)
// ---------------------------------------------------------------------------

/// WMTS Annex E GoogleCRS84Quad: level 0 is 2×1 tiles covering [-180,-90,180,90].
/// → resolution at level 0 = 360.0 / (2 * 256) = 0.703125 deg/px.
pub struct GlobalGeodetic {
    inner: QuadGrid,
}

impl GlobalGeodetic {
    pub fn new() -> Self {
        Self {
            inner: QuadGrid {
                name: "global_geodetic",
                crs: Crs::new("EPSG:4326"),
                tile_size: (256, 256),
                top_left: (-180.0, 90.0),
                base_resolution: 360.0 / 512.0,
                level_count: 20,
            },
        }
    }
}

impl Default for GlobalGeodetic {
    fn default() -> Self {
        Self::new()
    }
}

impl TileGrid for GlobalGeodetic {
    fn name(&self) -> &str {
        self.inner.name
    }
    fn crs(&self) -> &Crs {
        &self.inner.crs
    }
    fn origin(&self) -> Origin {
        Origin::TopLeft
    }
    fn tile_size(&self) -> (u32, u32) {
        self.inner.tile_size
    }
    fn resolution(&self, level: u32) -> f64 {
        self.inner.resolution(level)
    }
    fn level_count(&self) -> u32 {
        self.inner.level_count
    }
    fn tile_bbox(&self, coord: TileCoord) -> Bbox {
        self.inner.tile_bbox(coord)
    }
}

// ---------------------------------------------------------------------------
// RdNewQuad — NetherlandsRDNewQuad (EPSG:28992)
// ---------------------------------------------------------------------------

/// Standardized OGC pixel size (0.28 mm).
const OGC_PIXEL_SIZE_M: f64 = 0.28e-3;
/// PDOK NetherlandsRDNewQuad level-0 scale denominator.
const RDNEW_LEVEL0_SCALE_DENOM: f64 = 12288000.0;

pub struct RdNewQuad {
    inner: QuadGrid,
}

impl RdNewQuad {
    pub fn new() -> Self {
        Self {
            inner: QuadGrid {
                name: "rd_new_quad",
                crs: Crs::new("EPSG:28992"),
                tile_size: (256, 256),
                top_left: (-285401.92, 903401.92),
                base_resolution: OGC_PIXEL_SIZE_M * RDNEW_LEVEL0_SCALE_DENOM,
                level_count: 20,
            },
        }
    }
}

impl Default for RdNewQuad {
    fn default() -> Self {
        Self::new()
    }
}

impl TileGrid for RdNewQuad {
    fn name(&self) -> &str {
        self.inner.name
    }
    fn crs(&self) -> &Crs {
        &self.inner.crs
    }
    fn origin(&self) -> Origin {
        Origin::TopLeft
    }
    fn tile_size(&self) -> (u32, u32) {
        self.inner.tile_size
    }
    fn resolution(&self, level: u32) -> f64 {
        self.inner.resolution(level)
    }
    fn level_count(&self) -> u32 {
        self.inner.level_count
    }
    fn tile_bbox(&self, coord: TileCoord) -> Bbox {
        self.inner.tile_bbox(coord)
    }
}

// ---------------------------------------------------------------------------
// EtrsLaeaQuad — EuropeanETRS89_LAEAQuad (EPSG:3035)
// ---------------------------------------------------------------------------

/// OGC WMTS Annex E EuropeanETRS89_LAEAQuad places level 0 over an extent
/// of `[2000000, 1000000, 6500000, 5500000]` in EPSG:3035 — a square 4 500 000
/// meters on a side. At 256-pixel tiles, that gives a level-0 resolution of
/// `4_500_000 / 256 ≈ 17578.125` m/px and a top-left anchor at
/// `(2_000_000, 5_500_000)`.
//
// TODO: verify constants against the canonical OGC WMTS Annex E table; the
// values below match what's commonly quoted but should be cross-checked against
// the official spec before relying on them for capabilities output.
pub struct EtrsLaeaQuad {
    inner: QuadGrid,
}

impl EtrsLaeaQuad {
    pub fn new() -> Self {
        Self {
            inner: QuadGrid {
                name: "etrs89_laea_quad",
                crs: Crs::new("EPSG:3035"),
                tile_size: (256, 256),
                top_left: (2_000_000.0, 5_500_000.0),
                base_resolution: 4_500_000.0 / 256.0,
                level_count: 20,
            },
        }
    }
}

impl Default for EtrsLaeaQuad {
    fn default() -> Self {
        Self::new()
    }
}

impl TileGrid for EtrsLaeaQuad {
    fn name(&self) -> &str {
        self.inner.name
    }
    fn crs(&self) -> &Crs {
        &self.inner.crs
    }
    fn origin(&self) -> Origin {
        Origin::TopLeft
    }
    fn tile_size(&self) -> (u32, u32) {
        self.inner.tile_size
    }
    fn resolution(&self, level: u32) -> f64 {
        self.inner.resolution(level)
    }
    fn level_count(&self) -> u32 {
        self.inner.level_count
    }
    fn tile_bbox(&self, coord: TileCoord) -> Bbox {
        self.inner.tile_bbox(coord)
    }
}

// ---------------------------------------------------------------------------
// CustomGrid — wraps a `GridConfig` from `config.yaml`
// ---------------------------------------------------------------------------

/// A user-declared grid from `config.yaml`'s `grids:` section.
pub struct CustomGrid {
    name: String,
    cfg: GridConfig,
    level_count: u32,
}

impl CustomGrid {
    /// Wrap a [`GridConfig`] with the given name.
    ///
    /// Level count defaults to 24 — a generous power-of-two ladder that
    /// covers any plausible deep-zoom layer; callers can cap further via
    /// `Pyramid::max_level`.
    pub fn new(name: impl Into<String>, cfg: GridConfig) -> Self {
        Self::with_level_count(name, cfg, 24)
    }

    pub fn with_level_count(name: impl Into<String>, cfg: GridConfig, level_count: u32) -> Self {
        Self { name: name.into(), cfg, level_count }
    }
}

impl TileGrid for CustomGrid {
    fn name(&self) -> &str {
        &self.name
    }
    fn crs(&self) -> &Crs {
        &self.cfg.crs
    }
    fn origin(&self) -> Origin {
        match self.cfg.origin {
            GridOrigin::TopLeft => Origin::TopLeft,
            GridOrigin::BottomLeft => Origin::BottomLeft,
        }
    }
    fn tile_size(&self) -> (u32, u32) {
        (self.cfg.tile_size[0], self.cfg.tile_size[1])
    }
    fn resolution(&self, level: u32) -> f64 {
        self.cfg.base_resolution / (1u64 << level) as f64
    }
    fn level_count(&self) -> u32 {
        self.level_count
    }
    fn tile_bbox(&self, coord: TileCoord) -> Bbox {
        let res = self.resolution(coord.z);
        let tw = self.cfg.tile_size[0] as f64 * res;
        let th = self.cfg.tile_size[1] as f64 * res;
        let (tlx, tly) = (self.cfg.top_left[0], self.cfg.top_left[1]);
        match self.cfg.origin {
            GridOrigin::TopLeft => {
                let minx = tlx + coord.x as f64 * tw;
                let maxy = tly - coord.y as f64 * th;
                Bbox { minx, miny: maxy - th, maxx: minx + tw, maxy }
            }
            GridOrigin::BottomLeft => {
                // Interpret `top_left` as the (col=0, row=0) anchor — for
                // BottomLeft origin, that's the bottom-left corner.
                let minx = tlx + coord.x as f64 * tw;
                let miny = tly + coord.y as f64 * th;
                Bbox { minx, miny, maxx: minx + tw, maxy: miny + th }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Lookup
// ---------------------------------------------------------------------------

/// Resolve a grid name to a `TileGrid`, checking the built-in registry first
/// and then the user-supplied `custom` map.
pub fn lookup_grid(
    name: &str,
    custom: &HashMap<String, GridConfig>,
) -> Option<Arc<dyn TileGrid>> {
    match name {
        "webmercator" => Some(Arc::new(WebMercator::new())),
        "global_geodetic" => Some(Arc::new(GlobalGeodetic::new())),
        "rd_new_quad" => Some(Arc::new(RdNewQuad::new())),
        "etrs89_laea_quad" => Some(Arc::new(EtrsLaeaQuad::new())),
        _ => custom
            .get(name)
            .cloned()
            .map(|cfg| Arc::new(CustomGrid::new(name, cfg)) as Arc<dyn TileGrid>),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64, eps: f64) {
        assert!((a - b).abs() <= eps, "{a} !≈ {b} (eps={eps})");
    }

    #[test]
    fn webmercator_level0_tile_bbox_matches_world_extent() {
        let g = WebMercator::new();
        let b = g.tile_bbox(TileCoord::new(0, 0, 0));
        approx(b.minx, -20037508.34, 0.01);
        approx(b.miny, -20037508.34, 0.01);
        approx(b.maxx, 20037508.34, 0.01);
        approx(b.maxy, 20037508.34, 0.01);
    }

    #[test]
    fn webmercator_level0_resolution() {
        let g = WebMercator::new();
        approx(g.resolution(0), 156543.034, 0.001);
    }

    #[test]
    fn webmercator_halves_each_level() {
        let g = WebMercator::new();
        approx(g.resolution(1), g.resolution(0) / 2.0, 1e-9);
    }

    #[test]
    fn global_geodetic_level0_resolution_and_halving() {
        let g = GlobalGeodetic::new();
        approx(g.resolution(0), 0.703125, 1e-12);
        approx(g.resolution(1), g.resolution(0) / 2.0, 1e-12);
        // Level 0 covers two tiles horizontally → tile (0,0,0) ends at lon=0.
        let b = g.tile_bbox(TileCoord::new(0, 0, 0));
        approx(b.minx, -180.0, 1e-9);
        approx(b.maxx, 0.0, 1e-9);
        approx(b.maxy, 90.0, 1e-9);
        approx(b.miny, -90.0, 1e-9);
    }

    #[test]
    fn rd_new_quad_level0_top_left_and_halving() {
        let g = RdNewQuad::new();
        let b = g.tile_bbox(TileCoord::new(0, 0, 0));
        approx(b.minx, -285401.92, 1e-6);
        approx(b.maxy, 903401.92, 1e-6);
        approx(g.resolution(0), 3440.64, 1e-6);
        approx(g.resolution(1), g.resolution(0) / 2.0, 1e-9);
    }

    #[test]
    fn etrs_laea_halves_each_level() {
        let g = EtrsLaeaQuad::new();
        approx(g.resolution(1), g.resolution(0) / 2.0, 1e-9);
    }

    #[test]
    fn lookup_finds_builtins() {
        let custom = HashMap::new();
        assert!(lookup_grid("webmercator", &custom).is_some());
        assert!(lookup_grid("global_geodetic", &custom).is_some());
        assert!(lookup_grid("rd_new_quad", &custom).is_some());
        assert!(lookup_grid("etrs89_laea_quad", &custom).is_some());
        assert!(lookup_grid("does_not_exist", &custom).is_none());
    }

    #[test]
    fn lookup_finds_custom() {
        let mut custom = HashMap::new();
        custom.insert(
            "my_local".to_string(),
            GridConfig {
                crs: Crs::new("EPSG:31370"),
                top_left: [-100000.0, 500000.0],
                tile_size: [256, 256],
                base_resolution: 8192.0,
                origin: GridOrigin::TopLeft,
            },
        );
        let g = lookup_grid("my_local", &custom).expect("custom present");
        assert_eq!(g.name(), "my_local");
        assert_eq!(g.crs().as_str(), "EPSG:31370");
        approx(g.resolution(0), 8192.0, 1e-9);
        approx(g.resolution(1), 4096.0, 1e-9);
        let b = g.tile_bbox(TileCoord::new(0, 0, 0));
        approx(b.minx, -100000.0, 1e-9);
        approx(b.maxy, 500000.0, 1e-9);
    }
}
