//! WMTS 1.0.0 request parsing — stage 1 (syntax only, no config access).
//!
//! Two bindings, one output type ([`WmtsRequest`]):
//!
//! * KVP: `?SERVICE=WMTS&REQUEST=GetTile&LAYER=…`, decoded by the shared
//!   strict [`Kvp`] parser.
//! * RESTful: the path below `/wmts/`, in exactly the shape the capabilities
//!   advertise (see [`REST_TILE_TEMPLATE`]) — the same shape PDOK uses:
//!   `{Layer}/{TileMatrixSet}/{TileMatrix}/{TileCol}/{TileRow}.{ext}`,
//!   plus `1.0.0/WMTSCapabilities.xml` for GetCapabilities.
//!
//! This stage only checks shape: required parameters present, indices are
//! plain decimal numbers, format known. Whether the layer, TileMatrixSet,
//! style and tile exist is decided by [`super::validate`].

use crate::encode::OutputFormat;
use crate::protocol::error::{OgcCode, OgcError, Param};
use crate::protocol::kvp::Kvp;

/// The RESTful tile path below `{base}/wmts/{Layer}/`, as advertised in
/// `<ResourceURL template=…>` and accepted by [`parse_rest`]. Column before
/// row, no style segment — matching PDOK's `luchtfotorgb` WMTS.
pub const REST_TILE_TEMPLATE: &str = "{TileMatrixSet}/{TileMatrix}/{TileCol}/{TileRow}";

/// RESTful GetCapabilities path below `/wmts/`.
pub const REST_CAPABILITIES_PATH: &str = "1.0.0/WMTSCapabilities.xml";

/// A syntactically valid WMTS request (stage 1 output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WmtsRequest {
    GetCapabilities,
    GetTile(GetTileRequest),
    GetFeatureInfo,
}

/// GetTile parameters as sent; nothing here has been checked against the
/// catalog yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetTileRequest {
    pub layer: String,
    /// `STYLE` (KVP only; REST paths carry no style).
    pub style: Option<String>,
    pub tile_matrix_set: String,
    /// TileMatrix identifier, verified to be a plain decimal number.
    pub tile_matrix: u32,
    pub tile_row: u32,
    pub tile_col: u32,
    pub format: OutputFormat,
}

/// Parse a WMTS KVP query string (the part after `?`).
pub fn parse_kvp(query: &str) -> Result<WmtsRequest, OgcError> {
    let params = Kvp::parse(query)?;
    if let Some(service) = params.get(Param::Service) {
        if !service.eq_ignore_ascii_case("WMTS") {
            return Err(OgcError::invalid(Param::Service, "SERVICE must be WMTS"));
        }
    }
    if let Some(version) = params.get(Param::Version) {
        if version != "1.0.0" {
            return Err(OgcError::invalid(Param::Version, "VERSION must be 1.0.0"));
        }
    }
    let request = params.require(Param::Request)?;
    match request.to_ascii_lowercase().as_str() {
        "getcapabilities" => Ok(WmtsRequest::GetCapabilities),
        "gettile" => Ok(WmtsRequest::GetTile(GetTileRequest {
            layer: non_empty(&params, Param::Layer)?,
            style: params.get(Param::Style).map(str::to_string),
            tile_matrix_set: non_empty(&params, Param::TileMatrixSet)?,
            tile_matrix: index(params.require(Param::TileMatrix)?, Param::TileMatrix)?,
            tile_row: index(params.require(Param::TileRow)?, Param::TileRow)?,
            tile_col: index(params.require(Param::TileCol)?, Param::TileCol)?,
            format: format_from_mime(params.require(Param::Format)?)?,
        })),
        "getfeatureinfo" => Ok(WmtsRequest::GetFeatureInfo),
        _ => Err(OgcError::new(
            OgcCode::OperationNotSupported,
            Some(Param::Request),
            "REQUEST must be GetCapabilities, GetTile or GetFeatureInfo",
        )),
    }
}

/// Parse a WMTS RESTful path (below `/wmts/`; a leading `/` is tolerated).
pub fn parse_rest(path: &str) -> Result<WmtsRequest, OgcError> {
    const SHAPE: OgcError = OgcError::invalid(
        Param::Path,
        "path must be /wmts/{Layer}/{TileMatrixSet}/{TileMatrix}/{TileCol}/{TileRow}.{ext} \
         or /wmts/1.0.0/WMTSCapabilities.xml",
    );
    let path = path.strip_prefix('/').unwrap_or(path);
    if path == REST_CAPABILITIES_PATH {
        return Ok(WmtsRequest::GetCapabilities);
    }
    let segments: Vec<&str> = path.split('/').collect();
    let [layer, tile_matrix_set, tile_matrix, tile_col, last] = segments[..] else {
        return Err(SHAPE);
    };
    if [layer, tile_matrix_set, tile_matrix, tile_col, last]
        .iter()
        .any(|s| s.is_empty())
    {
        return Err(SHAPE);
    }
    let (tile_row, ext) = last.rsplit_once('.').ok_or(SHAPE)?;
    Ok(WmtsRequest::GetTile(GetTileRequest {
        layer: layer.to_string(),
        style: None,
        tile_matrix_set: tile_matrix_set.to_string(),
        tile_matrix: index(tile_matrix, Param::TileMatrix)?,
        tile_row: index(tile_row, Param::TileRow)?,
        tile_col: index(tile_col, Param::TileCol)?,
        format: format_from_ext(ext)?,
    }))
}

fn non_empty(params: &Kvp, param: Param) -> Result<String, OgcError> {
    let v = params.require(param)?;
    if v.is_empty() {
        return Err(OgcError::invalid(param, "parameter must not be empty"));
    }
    Ok(v.to_string())
}

/// A tile index or TileMatrix id: 1–10 ASCII digits (leading zeros allowed,
/// so PDOK-style `05` works), fitting in `u32`. No signs, spaces or exponents.
fn index(s: &str, param: Param) -> Result<u32, OgcError> {
    let bad = OgcError::invalid(param, "TileMatrix, TileRow and TileCol must be non-negative integers");
    if s.is_empty() || s.len() > 10 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad);
    }
    s.parse().map_err(|_| bad)
}

const BAD_FORMAT: OgcError = OgcError::new(OgcCode::InvalidFormat, Some(Param::Format), "format is not supported");

/// REST tile extension → format.
fn format_from_ext(ext: &str) -> Result<OutputFormat, OgcError> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Ok(OutputFormat::Png),
        "jpeg" | "jpg" => Ok(OutputFormat::Jpeg),
        "webp" => Ok(OutputFormat::Webp),
        "tif" | "tiff" => Ok(OutputFormat::GeoTiff),
        _ => Err(BAD_FORMAT),
    }
}

/// KVP `FORMAT` MIME type → format (media-type parameters ignored).
fn format_from_mime(mime: &str) -> Result<OutputFormat, OgcError> {
    let mt = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    match mt.as_str() {
        "image/png" => Ok(OutputFormat::Png),
        "image/jpeg" | "image/jpg" => Ok(OutputFormat::Jpeg),
        "image/webp" => Ok(OutputFormat::Webp),
        "image/tiff" | "image/geotiff" => Ok(OutputFormat::GeoTiff),
        _ => Err(BAD_FORMAT),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile(req: WmtsRequest) -> GetTileRequest {
        match req {
            WmtsRequest::GetTile(t) => t,
            other => panic!("expected GetTile, got {other:?}"),
        }
    }

    #[test]
    fn kvp_get_capabilities_any_key_case() {
        assert_eq!(
            parse_kvp("SERVICE=WMTS&REQUEST=GetCapabilities").unwrap(),
            WmtsRequest::GetCapabilities
        );
        assert_eq!(
            parse_kvp("service=wmts&request=getcapabilities").unwrap(),
            WmtsRequest::GetCapabilities
        );
    }

    #[test]
    fn kvp_get_tile_full() {
        let t = tile(
            parse_kvp(
                "SERVICE=WMTS&REQUEST=GetTile&VERSION=1.0.0&LAYER=Actueel_orthoHR&STYLE=default\
                 &TILEMATRIXSET=EPSG%3A28992&TILEMATRIX=05&TILEROW=12&TILECOL=34&FORMAT=image%2Fjpeg",
            )
            .unwrap(),
        );
        assert_eq!(t.layer, "Actueel_orthoHR");
        assert_eq!(t.style.as_deref(), Some("default"));
        assert_eq!(t.tile_matrix_set, "EPSG:28992");
        assert_eq!((t.tile_matrix, t.tile_row, t.tile_col), (5, 12, 34));
        assert_eq!(t.format, OutputFormat::Jpeg);
    }

    #[test]
    fn kvp_errors_carry_codes_and_locators() {
        let e = parse_kvp("SERVICE=WMTS").unwrap_err();
        assert_eq!(
            (e.code, e.locator_name()),
            (OgcCode::MissingParameterValue, Some("REQUEST"))
        );
        let e = parse_kvp("SERVICE=WMS&REQUEST=GetCapabilities").unwrap_err();
        assert_eq!(e.locator_name(), Some("SERVICE"));
        let e = parse_kvp("REQUEST=GetTile&LAYER=a&TILEMATRIXSET=t&TILEMATRIX=+1&TILEROW=0&TILECOL=0&FORMAT=image/png")
            .unwrap_err();
        assert_eq!(e.locator_name(), Some("TILEMATRIX"));
        let e = parse_kvp("REQUEST=GetTile&LAYER=a&TILEMATRIXSET=t&TILEMATRIX=1&TILEROW=0&TILECOL=0&FORMAT=image/gif")
            .unwrap_err();
        assert_eq!(e.code, OgcCode::InvalidFormat);
        let e = parse_kvp("REQUEST=GetTile&LAYER=&TILEMATRIXSET=t&TILEMATRIX=1&TILEROW=0&TILECOL=0&FORMAT=image/png")
            .unwrap_err();
        assert_eq!(e.locator_name(), Some("LAYER"));
        assert!(parse_kvp("REQUEST=GetCapabilities&VERSION=2.0.0").is_err());
    }

    #[test]
    fn rest_follows_the_advertised_template_column_before_row() {
        let t = tile(parse_rest("Actueel_orthoHR/EPSG:28992/05/34/12.jpeg").unwrap());
        assert_eq!(t.layer, "Actueel_orthoHR");
        assert_eq!(t.tile_matrix_set, "EPSG:28992");
        assert_eq!(t.tile_matrix, 5);
        assert_eq!((t.tile_col, t.tile_row), (34, 12), "TileCol comes before TileRow");
        assert_eq!(t.style, None);
        assert_eq!(
            parse_rest("/ortho/GoogleMapsCompatible/0/0/0.png").unwrap(),
            parse_rest("ortho/GoogleMapsCompatible/0/0/0.png").unwrap()
        );
    }

    #[test]
    fn rest_capabilities_and_extensions() {
        assert_eq!(
            parse_rest("1.0.0/WMTSCapabilities.xml").unwrap(),
            WmtsRequest::GetCapabilities
        );
        for (ext, fmt) in [
            ("png", OutputFormat::Png),
            ("jpg", OutputFormat::Jpeg),
            ("webp", OutputFormat::Webp),
            ("tif", OutputFormat::GeoTiff),
        ] {
            assert_eq!(tile(parse_rest(&format!("l/t/0/0/0.{ext}")).unwrap()).format, fmt);
        }
    }

    #[test]
    fn rest_rejects_other_shapes() {
        for p in [
            "too/short.png",
            "l/default/t/0/0/0.png", // old style-segment form
            "l/t/0/0/0",             // no extension
            "l/t/0/0/0.gif",
            "l//0/0/0.png",
            "l/t/0/0/0.png/",
            "l/t/x/0/0.png",
            "l/t/0/-1/0.png",
            "l/t/0/0/99999999999.png",
        ] {
            assert!(parse_rest(p).is_err(), "{p}");
        }
    }

    #[test]
    fn get_feature_info_is_recognised() {
        assert_eq!(
            parse_kvp("REQUEST=GetFeatureInfo").unwrap(),
            WmtsRequest::GetFeatureInfo
        );
    }
}
