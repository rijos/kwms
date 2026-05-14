//! Cache key: `(layer, z, x, y, format, crs)`.

use crate::crs::Crs;
use crate::encode::OutputFormat;
use crate::tile::TileCoord;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TileKey {
    pub layer: String,
    pub coord: TileCoord,
    pub format: OutputFormat,
    pub crs: Crs,
}

impl TileKey {
    /// Serialize as a forward-slash path suitable for OpenDAL.
    pub fn as_path(&self) -> String {
        todo!("phase 2 (cache agent)")
    }
}
