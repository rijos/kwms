//! `TileQuery` — a validated single-tile request, shared by WMTS (KVP and
//! REST) and TMS.
//!
//! Only the protocol validators can build one (the constructor is
//! `pub(in crate::protocol)`), and the constructor itself enforces the rules
//! every tile request must satisfy, so no protocol can forget one:
//!
//! | Rule | Error |
//! |---|---|
//! | level < the grid's level count | `TileOutOfRange` (TILEMATRIX) |
//! | column < matrix width at that level | `TileOutOfRange` (TILECOL) |
//! | row < matrix height at that level | `TileOutOfRange` (TILEROW) |
//! | format is offered by the layer | `InvalidFormat` (FORMAT) |
//!
//! Layer and grid are catalog entries, so whatever is downstream of a
//! `TileQuery` only ever sees configured layers, grids and CRSes.

use super::catalog::{CatalogGrid, CatalogLayer};
use super::error::{OgcCode, OgcError, Param};
use crate::encode::OutputFormat;
use crate::tile::TileCoord;

#[derive(Clone, Copy)]
pub struct TileQuery<'c> {
    layer: &'c CatalogLayer,
    grid: &'c CatalogGrid,
    coord: TileCoord,
    format: OutputFormat,
}

impl<'c> TileQuery<'c> {
    /// `coord` is top-down (row 0 = top), the internal convention.
    pub(in crate::protocol) fn new(
        layer: &'c CatalogLayer,
        grid: &'c CatalogGrid,
        coord: TileCoord,
        format: OutputFormat,
    ) -> Result<Self, OgcError> {
        debug_assert!(
            layer.grids().iter().any(|g| std::ptr::eq(g, grid)),
            "grid must be one of the layer's grids"
        );
        let g = grid.grid();
        if coord.z >= g.level_count() {
            return Err(OgcError::new(
                OgcCode::TileOutOfRange,
                Some(Param::TileMatrix),
                "TileMatrix is outside the TileMatrixSet",
            ));
        }
        let (cols, rows) = g.matrix_size(coord.z);
        if u64::from(coord.x) >= cols {
            return Err(OgcError::new(
                OgcCode::TileOutOfRange,
                Some(Param::TileCol),
                "TileCol is outside the TileMatrix",
            ));
        }
        if u64::from(coord.y) >= rows {
            return Err(OgcError::new(
                OgcCode::TileOutOfRange,
                Some(Param::TileRow),
                "TileRow is outside the TileMatrix",
            ));
        }
        if !layer.offers(format) {
            return Err(OgcError::new(
                OgcCode::InvalidFormat,
                Some(Param::Format),
                "format is not offered for this layer",
            ));
        }
        Ok(Self {
            layer,
            grid,
            coord,
            format,
        })
    }

    pub fn layer(&self) -> &'c CatalogLayer {
        self.layer
    }
    pub fn grid(&self) -> &'c CatalogGrid {
        self.grid
    }
    pub fn coord(&self) -> TileCoord {
        self.coord
    }
    pub fn format(&self) -> OutputFormat {
        self.format
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, LayerConfig};
    use crate::protocol::catalog::Catalog;

    pub(crate) fn catalog() -> Catalog {
        let layer: LayerConfig = serde_yaml::from_str(
            "name: ortho\ntitle: Ortho\ngrids: [webmercator]\nsources: [{uri: memory://x}]\nformats: [png]\n",
        )
        .unwrap();
        Catalog::build(&Config {
            layers: vec![layer],
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn constructor_enforces_range_and_format() {
        let cat = catalog();
        let layer = cat.layer("ortho").unwrap();
        let grid = layer.primary_grid();
        let ok = TileQuery::new(layer, grid, TileCoord::new(2, 3, 3), OutputFormat::Png).unwrap();
        assert_eq!(ok.coord(), TileCoord::new(2, 3, 3));

        let err = |c, f| TileQuery::new(layer, grid, c, f).err().unwrap();
        let e = err(TileCoord::new(22, 0, 0), OutputFormat::Png);
        assert_eq!(
            (e.code, e.locator_name()),
            (OgcCode::TileOutOfRange, Some("TILEMATRIX"))
        );
        let e = err(TileCoord::new(2, 4, 0), OutputFormat::Png);
        assert_eq!(e.locator_name(), Some("TILECOL"));
        let e = err(TileCoord::new(2, 0, 4), OutputFormat::Png);
        assert_eq!(e.locator_name(), Some("TILEROW"));
        let e = err(TileCoord::new(0, 0, 0), OutputFormat::Jpeg);
        assert_eq!(e.code, OgcCode::InvalidFormat);
    }
}
