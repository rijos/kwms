//! TMS 1.0.0 request parsing — stage 1 (syntax only, no config access).
//!
//! Paths below `/tms` (the route strips that prefix):
//!
//! * `""` or `"/"` → [`TmsRequest::Root`]
//! * `"/1.0.0"` or `"/1.0.0/"` → [`TmsRequest::ServiceMetadata`]
//! * `"/1.0.0/{layer}"` or `"/1.0.0/{layer}/"` → [`TmsRequest::TileMapResource`]
//! * `"/1.0.0/{layer}/{z}/{x}/{y}.{ext}"` → [`TmsRequest::GetTile`]
//!
//! Indices are plain decimal numbers. `y` is kept as written — TMS rows
//! count **bottom-up**; [`super::validate`] converts to the internal
//! top-down convention.

use crate::encode::OutputFormat;
use crate::protocol::error::{OgcCode, OgcError, Param};

/// A syntactically valid TMS request (stage 1 output).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmsRequest {
    Root,
    ServiceMetadata,
    TileMapResource {
        layer: String,
    },
    /// `y` is bottom-up, as on the wire.
    GetTile {
        layer: String,
        z: u32,
        x: u32,
        y: u32,
        format: OutputFormat,
    },
}

pub fn parse_path(path: &str) -> Result<TmsRequest, OgcError> {
    const SHAPE: OgcError = OgcError::invalid(Param::Path, "path must be /tms/1.0.0[/{layer}[/{z}/{x}/{y}.{ext}]]");
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    if trimmed.is_empty() {
        return Ok(TmsRequest::Root);
    }
    // A trailing slash is allowed on directory-like documents only.
    let (body, trailing_slash) = match trimmed.strip_suffix('/') {
        Some(b) => (b, true),
        None => (trimmed, false),
    };
    let segments: Vec<&str> = body.split('/').collect();
    if segments.iter().any(|s| s.is_empty()) {
        return Err(SHAPE);
    }
    if segments[0] != "1.0.0" {
        return Err(OgcError::invalid(Param::Version, "TMS version must be 1.0.0"));
    }
    match segments[..] {
        [_] => Ok(TmsRequest::ServiceMetadata),
        [_, layer] => Ok(TmsRequest::TileMapResource {
            layer: layer.to_string(),
        }),
        [_, layer, z, x, last] if !trailing_slash => {
            let (y, ext) = last.rsplit_once('.').ok_or(SHAPE)?;
            Ok(TmsRequest::GetTile {
                layer: layer.to_string(),
                z: index(z, Param::Z)?,
                x: index(x, Param::X)?,
                y: index(y, Param::Y)?,
                format: format_from_ext(ext)?,
            })
        }
        _ => Err(SHAPE),
    }
}

/// 1–10 ASCII digits fitting in `u32`.
fn index(s: &str, param: Param) -> Result<u32, OgcError> {
    let bad = OgcError::invalid(param, "z, x and y must be non-negative integers");
    if s.is_empty() || s.len() > 10 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad);
    }
    s.parse().map_err(|_| bad)
}

fn format_from_ext(ext: &str) -> Result<OutputFormat, OgcError> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Ok(OutputFormat::Png),
        "jpeg" | "jpg" => Ok(OutputFormat::Jpeg),
        "webp" => Ok(OutputFormat::Webp),
        "tif" | "tiff" => Ok(OutputFormat::GeoTiff),
        _ => Err(OgcError::new(
            OgcCode::InvalidFormat,
            Some(Param::Format),
            "tile extension is not supported",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn documents() {
        assert_eq!(parse_path("").unwrap(), TmsRequest::Root);
        assert_eq!(parse_path("/").unwrap(), TmsRequest::Root);
        assert_eq!(parse_path("/1.0.0").unwrap(), TmsRequest::ServiceMetadata);
        assert_eq!(parse_path("/1.0.0/").unwrap(), TmsRequest::ServiceMetadata);
        let tm = TmsRequest::TileMapResource {
            layer: "ortho2024".into(),
        };
        assert_eq!(parse_path("/1.0.0/ortho2024").unwrap(), tm);
        assert_eq!(parse_path("/1.0.0/ortho2024/").unwrap(), tm);
    }

    #[test]
    fn get_tile_keeps_wire_y() {
        assert_eq!(
            parse_path("/1.0.0/ortho2024/10/512/600.png").unwrap(),
            TmsRequest::GetTile {
                layer: "ortho2024".into(),
                z: 10,
                x: 512,
                y: 600,
                format: OutputFormat::Png
            }
        );
        for (ext, fmt) in [
            ("jpg", OutputFormat::Jpeg),
            ("webp", OutputFormat::Webp),
            ("tiff", OutputFormat::GeoTiff),
        ] {
            match parse_path(&format!("/1.0.0/l/0/0/0.{ext}")).unwrap() {
                TmsRequest::GetTile { format, .. } => assert_eq!(format, fmt),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn rejects_malformed_paths() {
        assert_eq!(parse_path("/0.9.0/foo").unwrap_err().locator_name(), Some("VERSION"));
        for p in [
            "/1.0.0/layer/0/0/no_ext",
            "/1.0.0/layer/a/0/0.png",
            "/1.0.0/layer/+1/0/0.png",
            "/1.0.0/a/b/c",
            "/1.0.0//0/0/0.png",
            "/1.0.0/layer/0/0/0.png/",
            "/1.0.0/layer/0/0/0.gif",
        ] {
            assert!(parse_path(p).is_err(), "{p}");
        }
    }
}
