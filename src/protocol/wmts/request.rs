//! WMTS 1.0.0 request parsing — KVP and RESTful bindings.
//!
//! Two entry points:
//!
//! * [`parse_kvp`] — parses the `query` portion of a WMTS KVP URL
//!   (`?SERVICE=WMTS&REQUEST=GetTile&LAYER=...`).
//! * [`parse_rest`] — parses the RESTful path shape
//!   `/{layer}/{style}/{tile_matrix_set}/{tile_matrix}/{tile_row}/{tile_col}.{ext}`.
//!
//! Both parsers produce the same [`WmtsRequest`] enum so the route handler can
//! dispatch uniformly. Parsing is purely syntactic — semantic validation
//! (layer exists, tile_matrix_set is advertised, etc.) is done by the route
//! handler against the live `Config`.

use anyhow::{anyhow, bail, Context, Result};

use crate::encode::OutputFormat;

/// A parsed WMTS request, regardless of binding (KVP vs RESTful).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WmtsRequest {
    GetCapabilities,
    GetTile(GetTileRequest),
    GetFeatureInfo(GetFeatureInfoRequest),
}

/// Fully parsed `GetTile` parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetTileRequest {
    pub layer: String,
    /// Style identifier — usually `"default"` in kwms.
    pub style: String,
    /// Advertised TileMatrixSet name (e.g. `"GoogleMapsCompatible"`).
    pub tile_matrix_set: String,
    /// Level identifier as advertised in capabilities (e.g. `"10"`).
    pub tile_matrix: String,
    pub tile_row: u32,
    pub tile_col: u32,
    pub format: OutputFormat,
}

/// `GetFeatureInfo` — same as `GetTile` plus pixel offset and info format.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetFeatureInfoRequest {
    pub get_tile: GetTileRequest,
    pub i: u32,
    pub j: u32,
    pub info_format: String,
}

/// Parse a WMTS KVP query string (the part after `?`).
///
/// Keys are case-insensitive per the WMTS spec; values are taken verbatim.
pub fn parse_kvp(query: &str) -> Result<WmtsRequest> {
    let params = parse_query_params(query);

    let service = params
        .get("service")
        .map(String::as_str)
        .unwrap_or("");
    if !service.eq_ignore_ascii_case("WMTS") && !service.is_empty() {
        bail!("WMTS KVP: unsupported SERVICE `{}` (expected WMTS)", service);
    }

    let request = params
        .get("request")
        .ok_or_else(|| anyhow!("WMTS KVP: missing REQUEST parameter"))?
        .to_string();

    match request.to_ascii_lowercase().as_str() {
        "getcapabilities" => Ok(WmtsRequest::GetCapabilities),
        "gettile" => Ok(WmtsRequest::GetTile(parse_kvp_get_tile(&params)?)),
        "getfeatureinfo" => {
            let get_tile = parse_kvp_get_tile(&params)?;
            let i = required_u32(&params, "i")?;
            let j = required_u32(&params, "j")?;
            let info_format = params
                .get("infoformat")
                .cloned()
                .ok_or_else(|| anyhow!("WMTS KVP GetFeatureInfo: missing INFOFORMAT"))?;
            Ok(WmtsRequest::GetFeatureInfo(GetFeatureInfoRequest {
                get_tile,
                i,
                j,
                info_format,
            }))
        }
        other => bail!("WMTS KVP: unsupported REQUEST `{}`", other),
    }
}

/// Parse a WMTS RESTful path:
/// `/{layer}/{style}/{tile_matrix_set}/{tile_matrix}/{tile_row}/{tile_col}.{ext}`
///
/// A leading `/` is tolerated.
pub fn parse_rest(path: &str) -> Result<WmtsRequest> {
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);

    let segments: Vec<&str> = trimmed.split('/').collect();
    if segments.len() != 6 {
        bail!(
            "WMTS REST: expected 6 path segments, got {} (`{}`)",
            segments.len(),
            path
        );
    }

    let layer = segments[0].to_string();
    let style = segments[1].to_string();
    let tile_matrix_set = segments[2].to_string();
    let tile_matrix = segments[3].to_string();
    let tile_row: u32 = segments[4]
        .parse()
        .with_context(|| format!("WMTS REST: bad TileRow `{}`", segments[4]))?;

    let last = segments[5];
    let (col_str, ext) = last
        .rsplit_once('.')
        .ok_or_else(|| anyhow!("WMTS REST: final segment `{}` missing `.ext`", last))?;
    let tile_col: u32 = col_str
        .parse()
        .with_context(|| format!("WMTS REST: bad TileCol `{}`", col_str))?;

    let format = format_from_ext(ext)?;

    Ok(WmtsRequest::GetTile(GetTileRequest {
        layer,
        style,
        tile_matrix_set,
        tile_matrix,
        tile_row,
        tile_col,
        format,
    }))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn parse_kvp_get_tile(
    params: &std::collections::HashMap<String, String>,
) -> Result<GetTileRequest> {
    let layer = required(params, "layer")?;
    let style = params
        .get("style")
        .cloned()
        .unwrap_or_else(|| "default".to_string());
    let tile_matrix_set = required(params, "tilematrixset")?;
    let tile_matrix = required(params, "tilematrix")?;
    let tile_row = required_u32(params, "tilerow")?;
    let tile_col = required_u32(params, "tilecol")?;
    let format_mime = required(params, "format")?;
    let format = format_from_mime(&format_mime)?;

    Ok(GetTileRequest {
        layer,
        style,
        tile_matrix_set,
        tile_matrix,
        tile_row,
        tile_col,
        format,
    })
}

fn required(
    params: &std::collections::HashMap<String, String>,
    key: &str,
) -> Result<String> {
    params
        .get(key)
        .cloned()
        .ok_or_else(|| anyhow!("WMTS KVP: missing `{}` parameter", key.to_ascii_uppercase()))
}

fn required_u32(
    params: &std::collections::HashMap<String, String>,
    key: &str,
) -> Result<u32> {
    required(params, key)?
        .parse::<u32>()
        .with_context(|| format!("WMTS KVP: `{}` must be a non-negative integer", key))
}

/// Parse a `k=v&k=v` query string into a case-folded map (keys lowercased,
/// values left as-is). Percent-decoding is intentionally minimal: we decode
/// `+` to space and `%XX` escapes, which is sufficient for the parameters
/// WMTS uses.
fn parse_query_params(query: &str) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    let q = query.strip_prefix('?').unwrap_or(query);
    if q.is_empty() {
        return out;
    }
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };
        let key = percent_decode(&k.replace('+', " ")).to_ascii_lowercase();
        let value = percent_decode(&v.replace('+', " "));
        out.insert(key, value);
    }
    out
}

fn percent_decode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) =
                (hex_digit(bytes[i + 1]), hex_digit(bytes[i + 2]))
            {
                out.push((hi * 16 + lo) as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Map a tile-URL extension to an [`OutputFormat`]. Matches the canonical
/// extensions exposed by [`OutputFormat::extension`] (`png`, `jpeg`, `webp`,
/// `tif`).
fn format_from_ext(ext: &str) -> Result<OutputFormat> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Ok(OutputFormat::Png),
        "jpeg" | "jpg" => Ok(OutputFormat::Jpeg),
        "webp" => Ok(OutputFormat::Webp),
        "tif" | "tiff" => Ok(OutputFormat::GeoTiff),
        other => bail!("WMTS REST: unsupported tile extension `{}`", other),
    }
}

/// Map a MIME type from a KVP `FORMAT=` to an [`OutputFormat`].
fn format_from_mime(mime: &str) -> Result<OutputFormat> {
    // Compare on the trimmed, lowercased media type — ignore parameters.
    let mt = mime.split(';').next().unwrap_or("").trim().to_ascii_lowercase();
    match mt.as_str() {
        "image/png" => Ok(OutputFormat::Png),
        "image/jpeg" | "image/jpg" => Ok(OutputFormat::Jpeg),
        "image/webp" => Ok(OutputFormat::Webp),
        "image/tiff" | "image/tif" => Ok(OutputFormat::GeoTiff),
        other => bail!("WMTS KVP: unsupported FORMAT `{}`", other),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_kvp_get_capabilities() {
        let req = parse_kvp("SERVICE=WMTS&REQUEST=GetCapabilities&VERSION=1.0.0")
            .expect("parses");
        assert_eq!(req, WmtsRequest::GetCapabilities);
    }

    #[test]
    fn parse_kvp_get_capabilities_lowercase_keys() {
        let req = parse_kvp("service=wmts&request=getcapabilities").expect("parses");
        assert_eq!(req, WmtsRequest::GetCapabilities);
    }

    #[test]
    fn parse_kvp_get_tile_full() {
        let q = "SERVICE=WMTS&REQUEST=GetTile&VERSION=1.0.0\
                 &LAYER=ortho2024&STYLE=default&TILEMATRIXSET=GoogleMapsCompatible\
                 &TILEMATRIX=10&TILEROW=512&TILECOL=600&FORMAT=image/png";
        let req = parse_kvp(q).expect("parses");
        match req {
            WmtsRequest::GetTile(t) => {
                assert_eq!(t.layer, "ortho2024");
                assert_eq!(t.style, "default");
                assert_eq!(t.tile_matrix_set, "GoogleMapsCompatible");
                assert_eq!(t.tile_matrix, "10");
                assert_eq!(t.tile_row, 512);
                assert_eq!(t.tile_col, 600);
                assert_eq!(t.format, OutputFormat::Png);
            }
            other => panic!("expected GetTile, got {other:?}"),
        }
    }

    #[test]
    fn parse_kvp_rejects_missing_request() {
        let err = parse_kvp("SERVICE=WMTS&VERSION=1.0.0").unwrap_err();
        let s = format!("{err:#}");
        assert!(s.to_lowercase().contains("request"), "got: {s}");
    }

    #[test]
    fn parse_kvp_rejects_bad_service() {
        let err = parse_kvp("SERVICE=WMS&REQUEST=GetCapabilities").unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("WMS") || s.to_lowercase().contains("service"));
    }

    #[test]
    fn parse_rest_get_tile_all_fields() {
        let req = parse_rest(
            "/ortho2024/default/GoogleMapsCompatible/10/512/600.png",
        )
        .expect("parses");
        match req {
            WmtsRequest::GetTile(t) => {
                assert_eq!(t.layer, "ortho2024");
                assert_eq!(t.style, "default");
                assert_eq!(t.tile_matrix_set, "GoogleMapsCompatible");
                assert_eq!(t.tile_matrix, "10");
                assert_eq!(t.tile_row, 512);
                assert_eq!(t.tile_col, 600);
                assert_eq!(t.format, OutputFormat::Png);
            }
            other => panic!("expected GetTile, got {other:?}"),
        }
    }

    #[test]
    fn parse_rest_strips_leading_slash() {
        let a = parse_rest("ortho/default/GoogleMapsCompatible/0/0/0.jpeg").unwrap();
        let b = parse_rest("/ortho/default/GoogleMapsCompatible/0/0/0.jpeg").unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn parse_rest_maps_extensions() {
        for (ext, fmt) in [
            ("png", OutputFormat::Png),
            ("jpeg", OutputFormat::Jpeg),
            ("webp", OutputFormat::Webp),
            ("tif", OutputFormat::GeoTiff),
        ] {
            let path = format!("/l/default/Tm/0/0/0.{ext}");
            let req = parse_rest(&path).expect("parses");
            match req {
                WmtsRequest::GetTile(t) => assert_eq!(t.format, fmt),
                other => panic!("expected GetTile, got {other:?}"),
            }
        }
    }

    #[test]
    fn parse_rest_rejects_bad_shape() {
        assert!(parse_rest("/too/short.png").is_err());
        assert!(parse_rest("/a/b/c/d/e/no_ext").is_err());
        assert!(parse_rest("/a/b/c/d/notnum/0.png").is_err());
    }

    #[test]
    fn parse_kvp_get_feature_info() {
        let q = "SERVICE=WMTS&REQUEST=GetFeatureInfo&VERSION=1.0.0\
                 &LAYER=ortho2024&STYLE=default&TILEMATRIXSET=GoogleMapsCompatible\
                 &TILEMATRIX=10&TILEROW=1&TILECOL=2&FORMAT=image/png\
                 &I=128&J=64&INFOFORMAT=application/json";
        match parse_kvp(q).expect("parses") {
            WmtsRequest::GetFeatureInfo(gfi) => {
                assert_eq!(gfi.i, 128);
                assert_eq!(gfi.j, 64);
                assert_eq!(gfi.info_format, "application/json");
                assert_eq!(gfi.get_tile.layer, "ortho2024");
            }
            other => panic!("expected GetFeatureInfo, got {other:?}"),
        }
    }
}
