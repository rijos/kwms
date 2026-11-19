//! TMS 1.0.0 request validation — stage 2 (rules against the catalog).
//!
//! | Rule | Error (locator) |
//! |---|---|
//! | `{layer}` is a configured layer | `LayerNotDefined` (LAYER) |
//! | `z` is a level of the layer's primary grid | `TileOutOfRange` (TILEMATRIX) |
//! | `y` is a row at that level (bottom-up), then flipped to top-down | `TileOutOfRange` (TILEROW) |
//! | column, format | see [`TileQuery`] |
//!
//! TMS serves each layer on its first configured grid.

use super::request::TmsRequest;
use crate::protocol::catalog::{Catalog, CatalogLayer};
use crate::protocol::error::{OgcCode, OgcError, Param};
use crate::protocol::query::TileQuery;
use crate::tile::TileCoord;

/// A validated TMS request.
pub enum TmsQuery<'c> {
    Root,
    ServiceMetadata,
    TileMap(&'c CatalogLayer),
    GetTile(TileQuery<'c>),
}

pub fn validate(req: TmsRequest, catalog: &Catalog) -> Result<TmsQuery<'_>, OgcError> {
    match req {
        TmsRequest::Root => Ok(TmsQuery::Root),
        TmsRequest::ServiceMetadata => Ok(TmsQuery::ServiceMetadata),
        TmsRequest::TileMapResource { layer } => layer_named(catalog, &layer).map(TmsQuery::TileMap),
        TmsRequest::GetTile { layer, z, x, y, format } => {
            let layer = layer_named(catalog, &layer)?;
            let grid = layer.primary_grid();
            let g = grid.grid();
            if z >= g.level_count() {
                return Err(OgcError::new(
                    OgcCode::TileOutOfRange,
                    Some(Param::Z),
                    "zoom level is outside the layer's tile pyramid",
                ));
            }
            let rows = g.matrix_size(z).1;
            if u64::from(y) >= rows {
                return Err(OgcError::new(
                    OgcCode::TileOutOfRange,
                    Some(Param::Y),
                    "tile row is outside the tile pyramid at this zoom level",
                ));
            }
            // TMS rows count up from the bottom; internally row 0 is the top.
            let top_down = rows
                .checked_sub(1)
                .and_then(|last| last.checked_sub(u64::from(y)))
                .and_then(|v| u32::try_from(v).ok())
                .ok_or(OgcError::new(
                    OgcCode::TileOutOfRange,
                    Some(Param::Y),
                    "tile row is outside the tile pyramid at this zoom level",
                ))?;
            TileQuery::new(layer, grid, TileCoord::new(z, x, top_down), format).map(TmsQuery::GetTile)
        }
    }
}

fn layer_named<'c>(catalog: &'c Catalog, name: &str) -> Result<&'c CatalogLayer, OgcError> {
    catalog.layer(name).ok_or(OgcError::new(
        OgcCode::LayerNotDefined,
        Some(Param::Layer),
        "not a layer of this service",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, LayerConfig};
    use crate::protocol::tms::request::parse_path;

    fn catalog() -> Catalog {
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

    fn tile(cat: &Catalog, path: &str) -> Result<TileCoord, OgcError> {
        match validate(parse_path(path)?, cat)? {
            TmsQuery::GetTile(q) => Ok(q.coord()),
            _ => panic!("expected GetTile"),
        }
    }

    #[test]
    fn rows_are_flipped_to_top_down() {
        let cat = catalog();
        // Level 1 has 2 rows: TMS y=0 (bottom) is internal row 1.
        assert_eq!(tile(&cat, "/1.0.0/ortho/1/0/0.png").unwrap(), TileCoord::new(1, 0, 1));
        assert_eq!(tile(&cat, "/1.0.0/ortho/1/1/1.png").unwrap(), TileCoord::new(1, 1, 0));
    }

    #[test]
    fn each_rule_has_its_error() {
        let cat = catalog();
        let err = |p: &str| {
            let e = tile(&cat, p).unwrap_err();
            (e.code, e.locator_name())
        };
        assert_eq!(err("/1.0.0/nope/0/0/0.png").0, OgcCode::LayerNotDefined);
        assert_eq!(err("/1.0.0/ortho/22/0/0.png"), (OgcCode::TileOutOfRange, Some("Z")));
        assert_eq!(
            err("/1.0.0/ortho/64/0/0.png").0,
            OgcCode::TileOutOfRange,
            "no shift overflow"
        );
        assert_eq!(err("/1.0.0/ortho/1/0/2.png"), (OgcCode::TileOutOfRange, Some("Y")));
        assert_eq!(
            err("/1.0.0/ortho/1/2/0.png"),
            (OgcCode::TileOutOfRange, Some("TILECOL"))
        );
        assert_eq!(err("/1.0.0/ortho/0/0/0.jpg").0, OgcCode::InvalidFormat);
        assert!(
            matches!(validate(parse_path("/1.0.0/nope").unwrap(), &cat), Err(e) if e.code == OgcCode::LayerNotDefined)
        );
    }
}
