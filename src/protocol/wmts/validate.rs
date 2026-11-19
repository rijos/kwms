//! WMTS 1.0.0 request validation — stage 2 (rules against the catalog).
//!
//! | Rule | Error (locator) |
//! |---|---|
//! | GetFeatureInfo is not implemented | `OperationNotSupported` (REQUEST) |
//! | `LAYER` is a configured layer | `LayerNotDefined` (LAYER) |
//! | `STYLE`, if given, is `default` or empty | `StyleNotDefined` (STYLE) |
//! | `TILEMATRIXSET` is one the layer is published on (advertised id or legacy alias) | `InvalidParameterValue` (TILEMATRIXSET) |
//! | TileMatrix/TileRow/TileCol inside the TileMatrixSet; format offered | see [`TileQuery`] |

use super::request::{GetTileRequest, WmtsRequest};
use crate::protocol::catalog::Catalog;
use crate::protocol::error::{OgcCode, OgcError, Param};
use crate::protocol::query::TileQuery;
use crate::tile::TileCoord;

/// A validated WMTS request.
pub enum WmtsQuery<'c> {
    GetCapabilities,
    GetTile(TileQuery<'c>),
}

pub fn validate(req: WmtsRequest, catalog: &Catalog) -> Result<WmtsQuery<'_>, OgcError> {
    match req {
        WmtsRequest::GetCapabilities => Ok(WmtsQuery::GetCapabilities),
        WmtsRequest::GetTile(t) => validate_get_tile(t, catalog).map(WmtsQuery::GetTile),
        WmtsRequest::GetFeatureInfo => Err(OgcError::new(
            OgcCode::OperationNotSupported,
            Some(Param::Request),
            "GetFeatureInfo is not supported",
        )),
    }
}

fn validate_get_tile(t: GetTileRequest, catalog: &Catalog) -> Result<TileQuery<'_>, OgcError> {
    let layer = catalog.layer(&t.layer).ok_or(OgcError::new(
        OgcCode::LayerNotDefined,
        Some(Param::Layer),
        "LAYER is not a layer of this service",
    ))?;
    if let Some(style) = t.style.as_deref() {
        if !(style.is_empty() || style == "default") {
            return Err(OgcError::new(
                OgcCode::StyleNotDefined,
                Some(Param::Style),
                "STYLE must be default",
            ));
        }
    }
    let grid = layer
        .grid_for_tile_matrix_set(&t.tile_matrix_set)
        .ok_or(OgcError::invalid(
            Param::TileMatrixSet,
            "the layer is not published on this TileMatrixSet",
        ))?;
    TileQuery::new(
        layer,
        grid,
        TileCoord::new(t.tile_matrix, t.tile_col, t.tile_row),
        t.format,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, LayerConfig};
    use crate::encode::OutputFormat;
    use crate::protocol::wmts::request::{parse_kvp, parse_rest};

    fn catalog() -> Catalog {
        let layer: LayerConfig = serde_yaml::from_str(
            "name: ortho\ntitle: Ortho\ngrids: [webmercator, rd_new_quad]\nsources: [{uri: memory://x}]\nformats: [jpeg]\n",
        )
        .unwrap();
        Catalog::build(&Config {
            layers: vec![layer],
            ..Default::default()
        })
        .unwrap()
    }

    fn rest(cat: &Catalog, path: &str) -> Result<(String, TileCoord, OutputFormat), OgcError> {
        match validate(parse_rest(path)?, cat)? {
            WmtsQuery::GetTile(q) => Ok((q.grid().name().to_string(), q.coord(), q.format())),
            WmtsQuery::GetCapabilities => panic!("expected GetTile"),
        }
    }

    #[test]
    fn valid_tile_resolves_to_catalog_entries() {
        let cat = catalog();
        let (grid, coord, fmt) = rest(&cat, "ortho/NetherlandsRDNewQuad/03/5/2.jpeg").unwrap();
        assert_eq!(grid, "rd_new_quad");
        assert_eq!(coord, TileCoord::new(3, 5, 2), "x = TileCol, y = TileRow");
        assert_eq!(fmt, OutputFormat::Jpeg);
    }

    #[test]
    fn each_rule_has_its_error() {
        let cat = catalog();
        let code = |path: &str| {
            let e = rest(&cat, path).unwrap_err();
            (e.code, e.locator_name())
        };
        assert_eq!(
            code("nope/GoogleMapsCompatible/0/0/0.jpeg"),
            (OgcCode::LayerNotDefined, Some("LAYER"))
        );
        assert_eq!(
            code("ortho/WorldCRS84Quad/0/0/0.jpeg"),
            (OgcCode::InvalidParameterValue, Some("TILEMATRIXSET"))
        );
        assert_eq!(
            code("ortho/webmercator/0/0/0.jpeg").1,
            Some("TILEMATRIXSET"),
            "grid names are not ids"
        );
        assert_eq!(
            code("ortho/GoogleMapsCompatible/22/0/0.jpeg"),
            (OgcCode::TileOutOfRange, Some("TILEMATRIX"))
        );
        assert_eq!(
            code("ortho/GoogleMapsCompatible/1/2/0.jpeg"),
            (OgcCode::TileOutOfRange, Some("TILECOL"))
        );
        assert_eq!(
            code("ortho/GoogleMapsCompatible/1/0/2.jpeg"),
            (OgcCode::TileOutOfRange, Some("TILEROW"))
        );
        assert_eq!(code("ortho/GoogleMapsCompatible/0/0/0.png").0, OgcCode::InvalidFormat);
    }

    #[test]
    fn kvp_style_and_feature_info() {
        let cat = catalog();
        let kvp = |style: &str| {
            parse_kvp(&format!(
                "REQUEST=GetTile&LAYER=ortho&STYLE={style}&TILEMATRIXSET=GoogleMapsCompatible\
                 &TILEMATRIX=0&TILEROW=0&TILECOL=0&FORMAT=image/jpeg"
            ))
            .and_then(|r| validate(r, &cat).map(|_| ()))
        };
        assert!(kvp("default").is_ok());
        assert!(kvp("").is_ok());
        assert_eq!(kvp("fancy").unwrap_err().code, OgcCode::StyleNotDefined);
        let e = validate(parse_kvp("REQUEST=GetFeatureInfo").unwrap(), &cat)
            .err()
            .unwrap();
        assert_eq!(e.code, OgcCode::OperationNotSupported);
    }
}
