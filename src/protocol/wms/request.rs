//! WMS request parsing (KVP query strings).
//!
//! Supports WMS 1.1.1 and 1.3.0. Both versions share the same request
//! catalog (`GetCapabilities`, `GetMap`, `GetFeatureInfo`) and differ
//! mainly in:
//!   - parameter name for the CRS (`SRS=` in 1.1.1 vs `CRS=` in 1.3.0);
//!   - BBOX axis order for geographic CRSes (1.3.0 honors the CRS's
//!     authority axis order — see [`super::axis_order`]);
//!   - feature-info pixel-coordinate parameter names (`X`/`Y` in 1.1.1,
//!     `I`/`J` in 1.3.0).
//!
//! Internally we always normalize to XY (kwms's `Bbox` is always
//! `minx,miny,maxx,maxy`) and to `I`/`J`.

use std::collections::HashMap;

use anyhow::{anyhow, bail, Context, Result};

use crate::crs::{Bbox, Crs};
use crate::encode::OutputFormat;

use super::axis_order::should_swap_axes;

/// WMS protocol version. Both 1.1.1 and 1.3.0 are required by spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WmsVersion {
    V1_1_1,
    V1_3_0,
}

impl WmsVersion {
    /// Parse the `VERSION=` wire value. Returns `None` for anything other
    /// than the two supported versions.
    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "1.1.1" => Some(Self::V1_1_1),
            "1.3.0" => Some(Self::V1_3_0),
            _ => None,
        }
    }

    /// Wire value, suitable for echoing back into capabilities / exceptions.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::V1_1_1 => "1.1.1",
            Self::V1_3_0 => "1.3.0",
        }
    }
}

/// A parsed WMS request, normalized to kwms's internal XY conventions.
#[derive(Debug, Clone)]
pub enum WmsRequest {
    GetCapabilities { version: WmsVersion },
    GetMap(GetMapRequest),
    GetFeatureInfo(GetFeatureInfoRequest),
}

#[derive(Debug, Clone)]
pub struct GetMapRequest {
    pub version: WmsVersion,
    pub layers: Vec<String>,
    pub styles: Vec<String>,
    pub crs: Crs,
    pub bbox: Bbox,
    pub width: u32,
    pub height: u32,
    pub format: OutputFormat,
    pub transparent: bool,
    pub bgcolor: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct GetFeatureInfoRequest {
    pub get_map: GetMapRequest,
    pub query_layers: Vec<String>,
    pub i: u32,
    pub j: u32,
    pub info_format: String,
    pub feature_count: u32,
}

/// Parse a KVP query string into a [`WmsRequest`].
///
/// Keys are case-insensitive (`SERVICE` and `service` both work); values
/// are taken verbatim except for the few well-known coercions (MIME type
/// → [`OutputFormat`], comma lists → `Vec<String>`, BBOX axis swap when
/// dictated by [`super::axis_order::should_swap_axes`]).
///
/// On any parsing or validation failure this returns an `anyhow::Error`
/// whose message is suitable for embedding in a `<ServiceException>`.
pub fn parse(query: &str) -> Result<WmsRequest> {
    let params = parse_kvp(query);

    // SERVICE — optional on GetCapabilities in 1.1.1, recommended overall.
    // We accept its absence on any request; if present, it must be "WMS".
    if let Some(svc) = params.get("service") {
        if !svc.eq_ignore_ascii_case("WMS") {
            bail!("unsupported SERVICE: '{svc}' (expected 'WMS')");
        }
    }

    let request = params
        .get("request")
        .ok_or_else(|| anyhow!("missing required REQUEST parameter"))?
        .to_string();

    match request.to_ascii_lowercase().as_str() {
        "getcapabilities" => parse_get_capabilities(&params),
        "getmap" => parse_get_map(&params).map(WmsRequest::GetMap),
        "getfeatureinfo" => parse_get_feature_info(&params).map(WmsRequest::GetFeatureInfo),
        other => bail!("unsupported REQUEST: '{other}'"),
    }
}

fn parse_get_capabilities(params: &HashMap<String, String>) -> Result<WmsRequest> {
    // VERSION on GetCapabilities is optional — clients use it to negotiate;
    // we default to 1.3.0 (the newer supported version) when absent.
    let version = match params.get("version") {
        Some(v) => WmsVersion::parse(v)
            .ok_or_else(|| anyhow!("unsupported VERSION: '{v}' (expected '1.1.1' or '1.3.0')"))?,
        None => WmsVersion::V1_3_0,
    };
    Ok(WmsRequest::GetCapabilities { version })
}

fn parse_get_map(params: &HashMap<String, String>) -> Result<GetMapRequest> {
    let version = parse_version(params)?;
    let layers = parse_comma_list(get_required(params, "layers")?);
    if layers.is_empty() {
        bail!("LAYERS must list at least one layer");
    }
    // STYLES is required by the WMS spec but may be empty; an empty value
    // means "default style for every layer".
    let styles = match params.get("styles") {
        Some(s) => parse_comma_list(s),
        None => Vec::new(),
    };

    let crs = parse_crs(params, version)?;
    let bbox_raw = parse_bbox(get_required(params, "bbox")?)?;
    let bbox = normalize_bbox(bbox_raw, &crs, version)?;

    let width: u32 = get_required(params, "width")?
        .parse()
        .context("WIDTH is not a valid u32")?;
    let height: u32 = get_required(params, "height")?
        .parse()
        .context("HEIGHT is not a valid u32")?;
    if width == 0 || height == 0 {
        bail!("WIDTH and HEIGHT must be positive");
    }

    let format_str = get_required(params, "format")?;
    let format = parse_format(format_str)?;

    let transparent = match params.get("transparent") {
        Some(v) => parse_bool_ish(v).context("TRANSPARENT must be TRUE or FALSE")?,
        None => false,
    };
    let bgcolor = match params.get("bgcolor") {
        Some(v) => Some(parse_bgcolor(v)?),
        None => None,
    };

    Ok(GetMapRequest {
        version,
        layers,
        styles,
        crs,
        bbox,
        width,
        height,
        format,
        transparent,
        bgcolor,
    })
}

fn parse_get_feature_info(params: &HashMap<String, String>) -> Result<GetFeatureInfoRequest> {
    // GetFeatureInfo embeds a full GetMap request (the same BBOX, CRS,
    // WIDTH/HEIGHT define the pixel grid we project I/J back through).
    let get_map = parse_get_map(params)?;

    let query_layers = parse_comma_list(get_required(params, "query_layers")?);
    if query_layers.is_empty() {
        bail!("QUERY_LAYERS must list at least one layer");
    }

    // 1.1.1 uses X/Y; 1.3.0 uses I/J. Accept the version-correct name.
    let (i_key, j_key) = match get_map.version {
        WmsVersion::V1_1_1 => ("x", "y"),
        WmsVersion::V1_3_0 => ("i", "j"),
    };
    let i: u32 = get_required(params, i_key)?
        .parse()
        .with_context(|| format!("{} is not a valid u32", i_key.to_ascii_uppercase()))?;
    let j: u32 = get_required(params, j_key)?
        .parse()
        .with_context(|| format!("{} is not a valid u32", j_key.to_ascii_uppercase()))?;

    let info_format = params
        .get("info_format")
        .cloned()
        .unwrap_or_else(|| "text/plain".to_string());

    let feature_count = match params.get("feature_count") {
        Some(v) => v.parse().context("FEATURE_COUNT is not a valid u32")?,
        None => 1,
    };

    Ok(GetFeatureInfoRequest {
        get_map,
        query_layers,
        i,
        j,
        info_format,
        feature_count,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Split `key=value&key=value` into a lowercase-keyed map. Values are
/// kept as-is (no URL-decoding; the HTTP layer is expected to deliver an
/// already-decoded query string, which is the case for `axum::extract::Query`
/// users who pass the raw map. Callers needing percent-decoding should
/// decode upstream).
fn parse_kvp(query: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        if let Some((k, v)) = pair.split_once('=') {
            out.insert(k.to_ascii_lowercase(), v.to_string());
        } else {
            // Bare key with no `=`; record an empty value so presence can
            // be detected.
            out.insert(pair.to_ascii_lowercase(), String::new());
        }
    }
    out
}

fn get_required<'a>(params: &'a HashMap<String, String>, key: &str) -> Result<&'a str> {
    params
        .get(key)
        .map(String::as_str)
        .ok_or_else(|| anyhow!("missing required {} parameter", key.to_ascii_uppercase()))
}

fn parse_version(params: &HashMap<String, String>) -> Result<WmsVersion> {
    let raw = get_required(params, "version")?;
    WmsVersion::parse(raw)
        .ok_or_else(|| anyhow!("unsupported VERSION: '{raw}' (expected '1.1.1' or '1.3.0')"))
}

fn parse_crs(params: &HashMap<String, String>, version: WmsVersion) -> Result<Crs> {
    // 1.1.1 → SRS=…, 1.3.0 → CRS=…. We're strict about which key is used
    // for which version because mixing them is a common client bug worth
    // surfacing as an error.
    let (preferred, alternate) = match version {
        WmsVersion::V1_1_1 => ("srs", "crs"),
        WmsVersion::V1_3_0 => ("crs", "srs"),
    };
    if let Some(v) = params.get(preferred) {
        return Ok(Crs::new(v.clone()));
    }
    if params.get(alternate).is_some() {
        bail!(
            "WMS {} uses {}= for CRS (got {}=)",
            version.as_str(),
            preferred.to_ascii_uppercase(),
            alternate.to_ascii_uppercase()
        );
    }
    bail!(
        "missing required {} parameter",
        preferred.to_ascii_uppercase()
    )
}

fn parse_comma_list(s: &str) -> Vec<String> {
    if s.is_empty() {
        return Vec::new();
    }
    s.split(',').map(|t| t.to_string()).collect()
}

fn parse_bbox(s: &str) -> Result<[f64; 4]> {
    let parts: Vec<&str> = s.split(',').collect();
    if parts.len() != 4 {
        bail!("BBOX must have 4 comma-separated values, got {}", parts.len());
    }
    let mut out = [0.0f64; 4];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p
            .trim()
            .parse()
            .with_context(|| format!("BBOX component {} ('{}') is not a number", i, p))?;
    }
    if !(out[0] < out[2]) {
        bail!("BBOX min/max along the first axis are inverted ({} >= {})", out[0], out[2]);
    }
    if !(out[1] < out[3]) {
        bail!("BBOX min/max along the second axis are inverted ({} >= {})", out[1], out[3]);
    }
    Ok(out)
}

/// Apply the version-appropriate axis swap and return a kwms `Bbox`
/// (always `minx,miny,maxx,maxy`).
fn normalize_bbox(raw: [f64; 4], crs: &Crs, version: WmsVersion) -> Result<Bbox> {
    if should_swap_axes(crs, version)? {
        // Wire is `lat_min, lon_min, lat_max, lon_max` → swap to XY.
        Ok(Bbox {
            minx: raw[1],
            miny: raw[0],
            maxx: raw[3],
            maxy: raw[2],
        })
    } else {
        Ok(Bbox {
            minx: raw[0],
            miny: raw[1],
            maxx: raw[2],
            maxy: raw[3],
        })
    }
}

fn parse_format(s: &str) -> Result<OutputFormat> {
    // FORMAT is a MIME type; we match exactly against the canonical
    // `OutputFormat::mime_type()` strings, accepting one common alias
    // (`image/tiff` for GeoTIFF — clients rarely send the
    // `; application=geotiff` parameter).
    let normalized = s.trim().to_ascii_lowercase();
    let f = match normalized.as_str() {
        "image/png" => OutputFormat::Png,
        "image/jpeg" => OutputFormat::Jpeg,
        "image/webp" => OutputFormat::Webp,
        "image/tiff" | "image/tiff; application=geotiff" | "image/geotiff" => {
            OutputFormat::GeoTiff
        }
        _ => bail!("unsupported FORMAT: '{s}'"),
    };
    Ok(f)
}

fn parse_bool_ish(s: &str) -> Result<bool> {
    match s.trim().to_ascii_lowercase().as_str() {
        "true" | "1" => Ok(true),
        "false" | "0" => Ok(false),
        _ => bail!("not a boolean: '{s}'"),
    }
}

fn parse_bgcolor(s: &str) -> Result<u32> {
    // WMS BGCOLOR is `0xRRGGBB`. We're lenient and also accept the bare
    // hex form (`RRGGBB`), since OpenLayers sometimes elides the prefix.
    let stripped = s
        .trim()
        .strip_prefix("0x")
        .or_else(|| s.trim().strip_prefix("0X"))
        .unwrap_or(s.trim());
    u32::from_str_radix(stripped, 16)
        .with_context(|| format!("BGCOLOR is not a valid hex color: '{s}'"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get_map(req: WmsRequest) -> GetMapRequest {
        match req {
            WmsRequest::GetMap(m) => m,
            other => panic!("expected GetMap, got {other:?}"),
        }
    }

    #[test]
    fn parse_get_capabilities_111() {
        let req = parse("SERVICE=WMS&REQUEST=GetCapabilities&VERSION=1.1.1").unwrap();
        match req {
            WmsRequest::GetCapabilities { version } => assert_eq!(version, WmsVersion::V1_1_1),
            other => panic!("expected GetCapabilities, got {other:?}"),
        }
    }

    #[test]
    fn parse_get_capabilities_130_lowercase_keys() {
        let req = parse("service=wms&request=GetCapabilities&version=1.3.0").unwrap();
        match req {
            WmsRequest::GetCapabilities { version } => assert_eq!(version, WmsVersion::V1_3_0),
            other => panic!("expected GetCapabilities, got {other:?}"),
        }
    }

    #[test]
    fn parse_get_map_111_uses_srs_and_xy_bbox() {
        let q = "SERVICE=WMS&VERSION=1.1.1&REQUEST=GetMap&LAYERS=ortho&STYLES=&\
                 SRS=EPSG:4326&BBOX=3.0,50.0,7.0,54.0&WIDTH=256&HEIGHT=256&FORMAT=image/png";
        let m = get_map(parse(q).unwrap());
        assert_eq!(m.version, WmsVersion::V1_1_1);
        assert_eq!(m.layers, vec!["ortho"]);
        assert_eq!(m.crs.as_str(), "EPSG:4326");
        // 1.1.1 BBOX is always XY (no swap).
        assert_eq!(m.bbox.minx, 3.0);
        assert_eq!(m.bbox.miny, 50.0);
        assert_eq!(m.bbox.maxx, 7.0);
        assert_eq!(m.bbox.maxy, 54.0);
        assert_eq!(m.format, OutputFormat::Png);
        assert_eq!(m.width, 256);
        assert_eq!(m.height, 256);
    }

    #[test]
    fn parse_get_map_130_swaps_geographic_bbox() {
        // 1.3.0 + EPSG:4326: BBOX on the wire is lat_min,lon_min,lat_max,lon_max.
        // After parsing it must be normalized to internal XY:
        //   minx = lon_min, miny = lat_min, maxx = lon_max, maxy = lat_max.
        // We only assert the swap when PROJ actually reports YX for 4326
        // (modern PROJ does; ancient builds may not, in which case the
        // wire layout is XY and no swap occurs).
        let q = "service=WMS&version=1.3.0&request=GetMap&layers=ortho&styles=&\
                 crs=EPSG:4326&bbox=50.0,3.0,54.0,7.0&width=256&height=256&format=image/png";
        let m = get_map(parse(q).unwrap());
        assert_eq!(m.version, WmsVersion::V1_3_0);

        let swap = should_swap_axes(&m.crs, WmsVersion::V1_3_0).unwrap();
        if swap {
            assert_eq!(m.bbox.minx, 3.0, "lon_min");
            assert_eq!(m.bbox.miny, 50.0, "lat_min");
            assert_eq!(m.bbox.maxx, 7.0, "lon_max");
            assert_eq!(m.bbox.maxy, 54.0, "lat_max");
        } else {
            assert_eq!(m.bbox.minx, 50.0);
            assert_eq!(m.bbox.miny, 3.0);
            assert_eq!(m.bbox.maxx, 54.0);
            assert_eq!(m.bbox.maxy, 7.0);
        }
    }

    #[test]
    fn parse_get_map_130_projected_crs_no_swap() {
        let q = "service=WMS&version=1.3.0&request=GetMap&layers=ortho&styles=&\
                 crs=EPSG:3857&bbox=0,0,1000,2000&width=256&height=256&format=image/png";
        let m = get_map(parse(q).unwrap());
        assert_eq!(m.bbox.minx, 0.0);
        assert_eq!(m.bbox.miny, 0.0);
        assert_eq!(m.bbox.maxx, 1000.0);
        assert_eq!(m.bbox.maxy, 2000.0);
    }

    #[test]
    fn parse_get_feature_info_130() {
        let q = "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetFeatureInfo&LAYERS=ortho&STYLES=&\
                 CRS=EPSG:3857&BBOX=0,0,1000,1000&WIDTH=256&HEIGHT=256&FORMAT=image/png&\
                 QUERY_LAYERS=ortho&I=128&J=64&INFO_FORMAT=application/json&FEATURE_COUNT=5";
        let req = parse(q).unwrap();
        match req {
            WmsRequest::GetFeatureInfo(f) => {
                assert_eq!(f.query_layers, vec!["ortho"]);
                assert_eq!(f.i, 128);
                assert_eq!(f.j, 64);
                assert_eq!(f.info_format, "application/json");
                assert_eq!(f.feature_count, 5);
                assert_eq!(f.get_map.layers, vec!["ortho"]);
            }
            other => panic!("expected GetFeatureInfo, got {other:?}"),
        }
    }

    #[test]
    fn parse_get_feature_info_111_uses_x_y() {
        let q = "SERVICE=WMS&VERSION=1.1.1&REQUEST=GetFeatureInfo&LAYERS=ortho&STYLES=&\
                 SRS=EPSG:3857&BBOX=0,0,1000,1000&WIDTH=256&HEIGHT=256&FORMAT=image/png&\
                 QUERY_LAYERS=ortho&X=10&Y=20";
        let req = parse(q).unwrap();
        match req {
            WmsRequest::GetFeatureInfo(f) => {
                assert_eq!(f.i, 10);
                assert_eq!(f.j, 20);
                assert_eq!(f.feature_count, 1, "default feature_count");
            }
            other => panic!("expected GetFeatureInfo, got {other:?}"),
        }
    }

    #[test]
    fn missing_required_param_errors() {
        // No BBOX → must fail.
        let q = "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=ortho&STYLES=&\
                 CRS=EPSG:3857&WIDTH=256&HEIGHT=256&FORMAT=image/png";
        let err = parse(q).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.to_ascii_lowercase().contains("bbox"),
            "expected error to mention BBOX, got: {msg}"
        );
    }

    #[test]
    fn unknown_version_errors() {
        let q = "SERVICE=WMS&VERSION=1.2.0&REQUEST=GetMap&LAYERS=ortho&STYLES=&\
                 CRS=EPSG:3857&BBOX=0,0,1,1&WIDTH=1&HEIGHT=1&FORMAT=image/png";
        let err = parse(q).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("1.2.0"), "expected error to mention bad version: {msg}");
    }

    #[test]
    fn unknown_format_errors() {
        let q = "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=ortho&STYLES=&\
                 CRS=EPSG:3857&BBOX=0,0,1,1&WIDTH=1&HEIGHT=1&FORMAT=image/gif";
        assert!(parse(q).is_err());
    }

    #[test]
    fn srs_in_130_errors() {
        // SRS= is the 1.1.1 spelling; in 1.3.0 the client must use CRS=.
        let q = "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=ortho&STYLES=&\
                 SRS=EPSG:3857&BBOX=0,0,1,1&WIDTH=1&HEIGHT=1&FORMAT=image/png";
        let err = parse(q).unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("CRS=") && msg.contains("SRS="),
            "expected error to mention both spellings: {msg}"
        );
    }
}
