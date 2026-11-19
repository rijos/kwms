//! WMS 1.1.1 / 1.3.0 request parsing — stage 1 (syntax only, no config
//! access, no GDAL).
//!
//! The query string is decoded by the shared strict [`Kvp`] parser, then
//! each parameter is checked for *shape*: present when required, numbers
//! that are numbers, a known VERSION and FORMAT. The CRS stays the string
//! the client sent — only [`super::validate`] may resolve it, against the
//! catalog. BBOX stays in wire axis order for the same reason (the order
//! depends on the CRS).

use crate::encode::OutputFormat;
use crate::protocol::error::{OgcCode, OgcError, Param};
use crate::protocol::kvp::Kvp;

/// WMS protocol version. Both 1.1.1 and 1.3.0 are supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WmsVersion {
    V1_1_1,
    V1_3_0,
}

impl WmsVersion {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "1.1.1" => Some(Self::V1_1_1),
            "1.3.0" => Some(Self::V1_3_0),
            _ => None,
        }
    }

    /// Wire value, for capabilities and exceptions.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1_1_1 => "1.1.1",
            Self::V1_3_0 => "1.3.0",
        }
    }

    /// The CRS parameter of this version (`SRS` / `CRS`).
    pub fn crs_param(self) -> Param {
        match self {
            Self::V1_1_1 => Param::Srs,
            Self::V1_3_0 => Param::Crs,
        }
    }
}

/// A syntactically valid WMS request (stage 1 output).
#[derive(Debug, Clone, PartialEq)]
pub enum WmsRequest {
    GetCapabilities { version: WmsVersion },
    GetMap(GetMapRequest),
    GetFeatureInfo { version: WmsVersion },
}

/// GetMap parameters as sent. Nothing here has been checked against the
/// catalog yet.
#[derive(Debug, Clone, PartialEq)]
pub struct GetMapRequest {
    pub version: WmsVersion,
    pub layers: Vec<String>,
    pub styles: Vec<String>,
    /// `CRS` (1.3.0) / `SRS` (1.1.1) exactly as sent.
    pub crs: String,
    /// BBOX in wire axis order: four finite numbers, min < max on each axis.
    pub bbox: [f64; 4],
    pub width: u32,
    pub height: u32,
    pub format: OutputFormat,
    pub transparent: bool,
    pub bgcolor: Option<u32>,
}

pub fn parse(query: &str) -> Result<WmsRequest, OgcError> {
    let params = Kvp::parse(query)?;
    if let Some(svc) = params.get(Param::Service) {
        if !svc.eq_ignore_ascii_case("WMS") {
            return Err(OgcError::invalid(Param::Service, "SERVICE must be WMS"));
        }
    }
    let request = params.require(Param::Request)?;
    match request.to_ascii_lowercase().as_str() {
        "getcapabilities" => {
            // VERSION is optional on GetCapabilities (negotiation); default 1.3.0.
            let version = match params.get(Param::Version) {
                Some(v) => version(v)?,
                None => WmsVersion::V1_3_0,
            };
            Ok(WmsRequest::GetCapabilities { version })
        }
        "getmap" => parse_get_map(&params).map(WmsRequest::GetMap),
        "getfeatureinfo" => Ok(WmsRequest::GetFeatureInfo {
            version: version(params.require(Param::Version)?)?,
        }),
        _ => Err(OgcError::new(
            OgcCode::OperationNotSupported,
            Some(Param::Request),
            "REQUEST must be GetCapabilities, GetMap or GetFeatureInfo",
        )),
    }
}

fn parse_get_map(params: &Kvp) -> Result<GetMapRequest, OgcError> {
    let version = version(params.require(Param::Version)?)?;
    let layers = list(params.require(Param::Layers)?);
    if layers.iter().all(String::is_empty) {
        return Err(OgcError::invalid(Param::Layers, "LAYERS must name a layer"));
    }
    // STYLES is required by the spec but widely omitted; absent = defaults.
    let styles = params.get(Param::Styles).map(list).unwrap_or_default();

    // 1.1.1 uses SRS=, 1.3.0 uses CRS=; the other spelling is a client bug.
    let (key, other, wrong_key) = match version {
        WmsVersion::V1_1_1 => (Param::Srs, Param::Crs, "WMS 1.1.1 uses SRS, not CRS"),
        WmsVersion::V1_3_0 => (Param::Crs, Param::Srs, "WMS 1.3.0 uses CRS, not SRS"),
    };
    let crs = match params.get(key) {
        Some(v) => v.to_string(),
        None if params.contains(other) => return Err(OgcError::invalid(other, wrong_key)),
        None => return Err(OgcError::missing(key)),
    };

    Ok(GetMapRequest {
        version,
        layers,
        styles,
        crs,
        bbox: bbox(params.require(Param::Bbox)?)?,
        width: dimension(params.require(Param::Width)?, Param::Width)?,
        height: dimension(params.require(Param::Height)?, Param::Height)?,
        format: format(params.require(Param::Format)?)?,
        transparent: match params.get(Param::Transparent) {
            Some(v) => boolean(v)?,
            None => false,
        },
        bgcolor: params.get(Param::BgColor).map(bgcolor).transpose()?,
    })
}

fn version(v: &str) -> Result<WmsVersion, OgcError> {
    WmsVersion::parse(v).ok_or(OgcError::invalid(Param::Version, "VERSION must be 1.1.1 or 1.3.0"))
}

/// Comma list; an empty string is an empty list.
fn list(s: &str) -> Vec<String> {
    if s.is_empty() {
        Vec::new()
    } else {
        s.split(',').map(str::to_string).collect()
    }
}

/// Four finite numbers, min < max on both axes (wire order).
fn bbox(s: &str) -> Result<[f64; 4], OgcError> {
    const BAD: OgcError = OgcError::invalid(Param::Bbox, "BBOX must be four finite numbers: minx,miny,maxx,maxy");
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        return Err(BAD);
    }
    let mut out = [0.0f64; 4];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p.trim().parse::<f64>().ok().filter(|v| v.is_finite()).ok_or(BAD)?;
    }
    if !(out[0] < out[2] && out[1] < out[3]) {
        return Err(OgcError::invalid(
            Param::Bbox,
            "BBOX minimum must be less than maximum on both axes",
        ));
    }
    Ok(out)
}

/// WIDTH/HEIGHT: 1–10 digits, at least 1 (the upper limit is a catalog rule).
fn dimension(s: &str, param: Param) -> Result<u32, OgcError> {
    let bad = OgcError::invalid(param, "WIDTH and HEIGHT must be positive integers");
    if s.is_empty() || s.len() > 10 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad);
    }
    s.parse::<u32>().ok().filter(|v| *v > 0).ok_or(bad)
}

/// FORMAT MIME type (exact, case-insensitive; `image/tiff` ≡ GeoTIFF).
fn format(s: &str) -> Result<OutputFormat, OgcError> {
    match s.trim().to_ascii_lowercase().as_str() {
        "image/png" => Ok(OutputFormat::Png),
        "image/jpeg" => Ok(OutputFormat::Jpeg),
        "image/webp" => Ok(OutputFormat::Webp),
        "image/tiff" | "image/tiff; application=geotiff" | "image/geotiff" => Ok(OutputFormat::GeoTiff),
        _ => Err(OgcError::new(
            OgcCode::InvalidFormat,
            Some(Param::Format),
            "FORMAT is not supported",
        )),
    }
}

fn boolean(s: &str) -> Result<bool, OgcError> {
    match s.to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => Err(OgcError::invalid(
            Param::Transparent,
            "TRANSPARENT must be TRUE or FALSE",
        )),
    }
}

/// `0xRRGGBB` (the bare `RRGGBB` some clients send is accepted too).
fn bgcolor(s: &str) -> Result<u32, OgcError> {
    const BAD: OgcError = OgcError::invalid(Param::BgColor, "BGCOLOR must be 0xRRGGBB");
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")).unwrap_or(s);
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(BAD);
    }
    u32::from_str_radix(hex, 16).map_err(|_| BAD)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAP_111: &str = "SERVICE=WMS&VERSION=1.1.1&REQUEST=GetMap&LAYERS=ortho&STYLES=&\
                           SRS=EPSG:4326&BBOX=3.0,50.0,7.0,54.0&WIDTH=256&HEIGHT=256&FORMAT=image/png";

    fn get_map(q: &str) -> GetMapRequest {
        match parse(q).unwrap() {
            WmsRequest::GetMap(m) => m,
            other => panic!("expected GetMap, got {other:?}"),
        }
    }

    fn err(q: &str) -> OgcError {
        parse(q).unwrap_err()
    }

    #[test]
    fn get_capabilities_versions() {
        assert_eq!(
            parse("SERVICE=WMS&REQUEST=GetCapabilities&VERSION=1.1.1").unwrap(),
            WmsRequest::GetCapabilities {
                version: WmsVersion::V1_1_1
            }
        );
        assert_eq!(
            parse("service=wms&request=GetCapabilities").unwrap(),
            WmsRequest::GetCapabilities {
                version: WmsVersion::V1_3_0
            }
        );
    }

    #[test]
    fn get_map_fields_as_sent() {
        let m = get_map(MAP_111);
        assert_eq!(m.version, WmsVersion::V1_1_1);
        assert_eq!(m.layers, vec!["ortho"]);
        assert_eq!(m.crs, "EPSG:4326", "CRS is kept verbatim for the validator");
        assert_eq!(m.bbox, [3.0, 50.0, 7.0, 54.0], "wire order, not swapped here");
        assert_eq!((m.width, m.height, m.format), (256, 256, OutputFormat::Png));
    }

    #[test]
    fn percent_decoded_once() {
        let m = get_map(
            "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=ortho&STYLES=&\
             CRS=EPSG%3A3857&BBOX=0%2C0%2C1000%2C1000&WIDTH=256&HEIGHT=256&FORMAT=image%2Fpng",
        );
        assert_eq!((m.crs.as_str(), m.bbox[2]), ("EPSG:3857", 1000.0));
    }

    #[test]
    fn errors_name_the_parameter() {
        let replace = |from: &str, to: &str| err(&MAP_111.replace(from, to));
        let e = replace("&BBOX=3.0,50.0,7.0,54.0", "");
        assert_eq!(
            (e.code, e.locator_name()),
            (OgcCode::MissingParameterValue, Some("BBOX"))
        );
        assert_eq!(
            replace("VERSION=1.1.1", "VERSION=1.2.0").locator_name(),
            Some("VERSION")
        );
        assert_eq!(replace("image/png", "image/gif").code, OgcCode::InvalidFormat);
        assert_eq!(replace("SRS=", "CRS=").locator_name(), Some("CRS"));
        assert_eq!(replace("LAYERS=ortho", "LAYERS=").locator_name(), Some("LAYERS"));
        for (from, to, loc) in [
            ("WIDTH=256", "WIDTH=0", "WIDTH"),
            ("WIDTH=256", "WIDTH=+5", "WIDTH"),
            ("HEIGHT=256", "HEIGHT=1e3", "HEIGHT"),
            ("3.0,50.0,7.0,54.0", "-inf,-inf,inf,inf", "BBOX"),
            ("3.0,50.0,7.0,54.0", "0,0,NaN,1", "BBOX"),
            ("3.0,50.0,7.0,54.0", "0,0,1e400,1", "BBOX"),
            ("3.0,50.0,7.0,54.0", "7,50,3,54", "BBOX"),
            ("3.0,50.0,7.0,54.0", "1,2,3", "BBOX"),
        ] {
            assert_eq!(replace(from, to).locator_name(), Some(loc), "{from} -> {to}");
        }
        assert!(err(&format!("{MAP_111}&BGCOLOR=0xFFF")).locator_name() == Some("BGCOLOR"));
        assert!(err(&format!("{MAP_111}&TRANSPARENT=maybe")).locator_name() == Some("TRANSPARENT"));
        assert_eq!(err(&format!("{MAP_111}&LAYERS=again")).locator_name(), Some("LAYERS"));
    }

    #[test]
    fn any_crs_string_survives_stage_one() {
        // Stage 1 never interprets the CRS; the catalog rejects unknown ones.
        let m = get_map(&MAP_111.replace("SRS=EPSG:4326", "SRS=/etc/passwd"));
        assert_eq!(m.crs, "/etc/passwd");
    }

    #[test]
    fn other_requests() {
        assert_eq!(
            parse("REQUEST=GetFeatureInfo&VERSION=1.3.0").unwrap(),
            WmsRequest::GetFeatureInfo {
                version: WmsVersion::V1_3_0
            }
        );
        assert_eq!(err("REQUEST=GetLegendGraphic").code, OgcCode::OperationNotSupported);
        assert_eq!(
            err("SERVICE=WMTS&REQUEST=GetCapabilities").locator_name(),
            Some("SERVICE")
        );
    }
}
