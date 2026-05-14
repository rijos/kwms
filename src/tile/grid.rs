//! `TileGrid` trait and grid implementations.

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
