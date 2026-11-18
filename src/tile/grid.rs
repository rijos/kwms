//! `TileGrid` trait and grid implementations.

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{GridConfig, GridOrigin};
use crate::crs::{Bbox, Crs};

use super::TileCoord;

/// A tile grid — CRS + tile size + resolutions ladder.
///
/// Every grid is addressed top-down internally: `(col 0, row 0)` is the
/// top-left tile of each level (the WMTS convention). Custom grids declared
/// with `origin: bottom_left` are converted to this form when built, and TMS
/// flips rows at its own boundary.
///
/// Well-known grids (`webmercator`, `global_geodetic`, `rd_new_quad`, …) are
/// built-in implementations of this trait. Custom grids declared in
/// `config.yaml` have their own implementation backed by `GridConfig`.
pub trait TileGrid: Send + Sync {
    /// Stable identifier (e.g. `"webmercator"`).
    fn name(&self) -> &str;
    /// CRS the grid is defined in.
    fn crs(&self) -> &Crs;
    /// (width, height) of a tile in pixels.
    fn tile_size(&self) -> (u32, u32);
    /// CRS-units-per-pixel at the given level.
    fn resolution(&self, level: u32) -> f64;
    /// Number of advertised levels.
    fn level_count(&self) -> u32;
    /// Bbox of a specific tile.
    fn tile_bbox(&self, coord: TileCoord) -> Bbox;
    /// `(columns, rows)` of the tile matrix at `level`.
    fn matrix_size(&self, level: u32) -> (u64, u64);

    /// Area covered by the full tile matrix (the same at every level).
    fn extent(&self) -> Bbox {
        let tl = self.tile_bbox(TileCoord::new(0, 0, 0));
        let (cols, rows) = self.matrix_size(0);
        Bbox {
            minx: tl.minx,
            maxy: tl.maxy,
            maxx: tl.minx + cols as f64 * (tl.maxx - tl.minx),
            miny: tl.maxy - rows as f64 * (tl.maxy - tl.miny),
        }
    }

    /// Ensure `coord` addresses a real tile: an advertised level and a
    /// column/row inside that level's matrix. Request handlers must call this
    /// before rendering so clients can't make the server render (and cache)
    /// tiles outside the pyramid.
    fn check_coord(&self, coord: TileCoord) -> Result<(), String> {
        if coord.z >= self.level_count() {
            return Err(format!(
                "tile level {} out of range for grid `{}` (levels 0..={})",
                coord.z,
                self.name(),
                self.level_count().saturating_sub(1)
            ));
        }
        let (cols, rows) = self.matrix_size(coord.z);
        if u64::from(coord.x) >= cols || u64::from(coord.y) >= rows {
            return Err(format!(
                "tile column/row ({}, {}) out of range at level {} (matrix is {cols}x{rows})",
                coord.x, coord.y, coord.z
            ));
        }
        Ok(())
    }
}

/// `2^level` as f64 without integer-shift overflow for any `level`.
fn pow2(level: u32) -> f64 {
    2f64.powi(level.min(i32::MAX as u32) as i32)
}

/// `base << level`, saturating instead of overflowing.
fn matrix_dim(base: u64, level: u32) -> u64 {
    base.checked_shl(level)
        .filter(|v| v >> level == base)
        .unwrap_or(u64::MAX)
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
    /// `(columns, rows)` at level 0 (2×1 for GoogleCRS84Quad, else 1×1).
    matrix0: (u64, u64),
}

impl QuadGrid {
    fn resolution(&self, level: u32) -> f64 {
        self.base_resolution / pow2(level)
    }

    fn matrix_size(&self, level: u32) -> (u64, u64) {
        (matrix_dim(self.matrix0.0, level), matrix_dim(self.matrix0.1, level))
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
                matrix0: (1, 1),
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
    fn matrix_size(&self, level: u32) -> (u64, u64) {
        self.inner.matrix_size(level)
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
                matrix0: (2, 1),
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
    fn matrix_size(&self, level: u32) -> (u64, u64) {
        self.inner.matrix_size(level)
    }
}

// ---------------------------------------------------------------------------
// RdNewQuad — NetherlandsRDNewQuad (EPSG:28992)
// ---------------------------------------------------------------------------

/// Standardized OGC pixel size (0.28 mm).
pub const OGC_PIXEL_SIZE_M: f64 = 0.28e-3;
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
                matrix0: (1, 1),
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
    fn matrix_size(&self, level: u32) -> (u64, u64) {
        self.inner.matrix_size(level)
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
                matrix0: (1, 1),
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
    fn matrix_size(&self, level: u32) -> (u64, u64) {
        self.inner.matrix_size(level)
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
    /// Top-left corner of the level-0 tile, after normalising the declared
    /// anchor (a `bottom_left` grid's `top_left` field is its bottom-left).
    top_left: (f64, f64),
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
        let [ax, ay] = cfg.top_left;
        // Custom grids are one tile at level 0, so the level-0 tile height
        // converts a bottom-left anchor into the equivalent top-left one.
        let top_left = match cfg.origin {
            GridOrigin::TopLeft => (ax, ay),
            GridOrigin::BottomLeft => (ax, ay + f64::from(cfg.tile_size[1]) * cfg.base_resolution),
        };
        Self { name: name.into(), cfg, level_count, top_left }
    }
}

impl TileGrid for CustomGrid {
    fn name(&self) -> &str {
        &self.name
    }
    fn crs(&self) -> &Crs {
        &self.cfg.crs
    }
    fn tile_size(&self) -> (u32, u32) {
        (self.cfg.tile_size[0], self.cfg.tile_size[1])
    }
    fn resolution(&self, level: u32) -> f64 {
        self.cfg.base_resolution / pow2(level)
    }
    fn matrix_size(&self, level: u32) -> (u64, u64) {
        // Custom grids are power-of-two ladders that are one tile at level 0
        // (the same assumption WMTS capabilities make when advertising them).
        (matrix_dim(1, level), matrix_dim(1, level))
    }
    fn level_count(&self) -> u32 {
        self.level_count
    }
    fn tile_bbox(&self, coord: TileCoord) -> Bbox {
        let res = self.resolution(coord.z);
        let tw = f64::from(self.cfg.tile_size[0]) * res;
        let th = f64::from(self.cfg.tile_size[1]) * res;
        let (tlx, tly) = self.top_left;
        let minx = tlx + f64::from(coord.x) * tw;
        let maxy = tly - f64::from(coord.y) * th;
        Bbox { minx, miny: maxy - th, maxx: minx + tw, maxy }
    }
}

// ---------------------------------------------------------------------------
// Lookup
// ---------------------------------------------------------------------------

/// A built-in grid and the names it is published under. This table is the
/// single source of truth for built-in grid names: config validation, grid
/// lookup, WMTS TileMatrixSet identifiers and TMS profiles all read it.
pub struct WellKnownGrid {
    /// Name used in `config.yaml` (`grids: [webmercator]`).
    pub name: &'static str,
    /// WMTS `TileMatrixSet` identifier.
    pub tile_matrix_set: &'static str,
    /// Older identifiers still accepted in WMTS requests.
    pub tile_matrix_set_aliases: &'static [&'static str],
    /// TMS `profile=` attribute.
    pub tms_profile: &'static str,
    build: fn() -> Arc<dyn TileGrid>,
}

pub const WELL_KNOWN_GRIDS: &[WellKnownGrid] = &[
    WellKnownGrid {
        name: "webmercator",
        tile_matrix_set: "GoogleMapsCompatible",
        tile_matrix_set_aliases: &[],
        tms_profile: "global-mercator",
        build: || Arc::new(WebMercator::new()),
    },
    WellKnownGrid {
        // 2×1 tiles at level 0, 0.703125°/px: OGC "WorldCRS84Quad" (and the
        // TMS global-geodetic profile). Earlier releases advertised it as
        // GoogleCRS84Quad, which is a different (1×1) layout; that name is
        // still accepted in requests.
        name: "global_geodetic",
        tile_matrix_set: "WorldCRS84Quad",
        tile_matrix_set_aliases: &["GoogleCRS84Quad"],
        tms_profile: "global-geodetic",
        build: || Arc::new(GlobalGeodetic::new()),
    },
    WellKnownGrid {
        name: "rd_new_quad",
        tile_matrix_set: "NetherlandsRDNewQuad",
        tile_matrix_set_aliases: &[],
        tms_profile: "local",
        build: || Arc::new(RdNewQuad::new()),
    },
    WellKnownGrid {
        name: "etrs89_laea_quad",
        tile_matrix_set: "EuropeanETRS89_LAEAQuad",
        tile_matrix_set_aliases: &[],
        tms_profile: "local",
        build: || Arc::new(EtrsLaeaQuad::new()),
    },
];

pub fn well_known_grid(name: &str) -> Option<&'static WellKnownGrid> {
    WELL_KNOWN_GRIDS.iter().find(|g| g.name == name)
}

/// WMTS TileMatrixSet identifier for a grid name (custom grids keep their name).
pub fn tile_matrix_set_name(grid_name: &str) -> &str {
    well_known_grid(grid_name).map_or(grid_name, |g| g.tile_matrix_set)
}

/// Grid name for a WMTS TileMatrixSet identifier (or legacy alias).
pub fn grid_name_for_tile_matrix_set(tms: &str) -> &str {
    WELL_KNOWN_GRIDS
        .iter()
        .find(|g| g.tile_matrix_set == tms || g.tile_matrix_set_aliases.contains(&tms))
        .map_or(tms, |g| g.name)
}

/// TMS `profile=` for a grid name (custom grids are `local`).
pub fn tms_profile(grid_name: &str) -> &'static str {
    well_known_grid(grid_name).map_or("local", |g| g.tms_profile)
}

/// Resolve a grid name to a `TileGrid`: built-in grids first, then the
/// user-supplied `custom` map. (Validation forbids custom grids that reuse a
/// built-in name, so the precedence never hides one.)
pub fn lookup_grid(
    name: &str,
    custom: &HashMap<String, GridConfig>,
) -> Option<Arc<dyn TileGrid>> {
    if let Some(g) = well_known_grid(name) {
        return Some((g.build)());
    }
    custom
        .get(name)
        .cloned()
        .map(|cfg| Arc::new(CustomGrid::new(name, cfg)) as Arc<dyn TileGrid>)
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

    #[test]
    fn check_coord_rejects_levels_and_indices_outside_the_pyramid() {
        let g = WebMercator::new();
        assert!(g.check_coord(TileCoord::new(0, 0, 0)).is_ok());
        assert!(g.check_coord(TileCoord::new(21, (1 << 21) - 1, (1 << 21) - 1)).is_ok());
        assert!(g.check_coord(TileCoord::new(22, 0, 0)).is_err(), "level past level_count");
        assert!(g.check_coord(TileCoord::new(64, 0, 0)).is_err(), "would overflow a shift");
        assert!(g.check_coord(TileCoord::new(u32::MAX, 0, 0)).is_err());
        assert!(g.check_coord(TileCoord::new(3, 8, 0)).is_err(), "column past matrix");
        assert!(g.check_coord(TileCoord::new(3, 0, 8)).is_err(), "row past matrix");

        let geo = GlobalGeodetic::new();
        assert_eq!(geo.matrix_size(0), (2, 1));
        assert!(geo.check_coord(TileCoord::new(0, 1, 0)).is_ok());
        assert!(geo.check_coord(TileCoord::new(0, 0, 1)).is_err());
    }

    #[test]
    fn resolution_does_not_panic_for_huge_levels() {
        let g = WebMercator::new();
        assert_eq!(g.resolution(64), g.resolution(0) / 2f64.powi(64));
        assert!(g.resolution(u32::MAX) >= 0.0);
        assert_eq!(super::matrix_dim(1, 64), u64::MAX);
        assert_eq!(super::matrix_dim(1, 63), 1u64 << 63);
        assert_eq!(super::matrix_dim(2, 63), u64::MAX);
    }

    #[test]
    fn bottom_left_custom_grid_is_addressed_top_down() {
        let cfg = |origin| GridConfig {
            crs: Crs::new("EPSG:28992"),
            top_left: [0.0, 0.0],
            tile_size: [256, 256],
            base_resolution: 1.0,
            origin,
        };
        // Bottom-left anchor (0,0) with a 256-unit level-0 tile is the same
        // grid as a top-left anchor at (0,256).
        let bl = CustomGrid::new("bl", cfg(GridOrigin::BottomLeft));
        let tl = CustomGrid::new("tl", GridConfig { top_left: [0.0, 256.0], ..cfg(GridOrigin::TopLeft) });
        for c in [TileCoord::new(0, 0, 0), TileCoord::new(1, 0, 0), TileCoord::new(1, 1, 1)] {
            assert_eq!(bl.tile_bbox(c), tl.tile_bbox(c), "{c:?}");
        }
        // Row 0 at level 1 is the *top* row.
        assert_eq!(bl.tile_bbox(TileCoord::new(1, 0, 0)).maxy, 256.0);
        assert_eq!(bl.extent(), Bbox::from([0.0, 0.0, 256.0, 256.0]));
    }

    #[test]
    fn registry_names_round_trip() {
        for g in WELL_KNOWN_GRIDS {
            assert!(lookup_grid(g.name, &HashMap::new()).is_some(), "{}", g.name);
            assert_eq!(grid_name_for_tile_matrix_set(g.tile_matrix_set), g.name);
            assert_eq!(tile_matrix_set_name(g.name), g.tile_matrix_set);
        }
        assert_eq!(grid_name_for_tile_matrix_set("GoogleCRS84Quad"), "global_geodetic");
        assert_eq!(tile_matrix_set_name("my_custom"), "my_custom");
        assert_eq!(tms_profile("webmercator"), "global-mercator");
        assert_eq!(tms_profile("my_custom"), "local");
    }
}
