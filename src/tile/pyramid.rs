//! Image pyramid — a grid + a max level resolved from `max_resolution`.

use std::sync::Arc;

use crate::crs::Bbox;

use super::TileGrid;

pub struct Pyramid {
    pub grid: Arc<dyn TileGrid>,
    pub max_level: u32,
}

impl Pyramid {
    /// Select the pyramid level whose resolution is the coarsest that still
    /// covers the requested (bbox, output size) without oversampling.
    pub fn select_level(&self, _bbox: &Bbox, _size: (u32, u32)) -> u32 {
        todo!("phase 2 (tile agent)")
    }
}
