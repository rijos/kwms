//! TMS 1.0.0 request parsing — path-only, no KVP.
//!
//! TMS exposes a hierarchy of documents and tile URLs under a fixed
//! `/tms/1.0.0/...` prefix. The route handler strips the `/tms` prefix before
//! calling [`parse_path`], so paths here begin with the version segment (or
//! are empty for the service root).

use anyhow::{anyhow, bail, Context, Result};

use crate::encode::OutputFormat;

/// A parsed TMS request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmsRequest {
    /// `/tms/` — service root listing all TMS versions.
    Root,
    /// `/tms/1.0.0/` — `<TileMapService>` document.
    ServiceMetadata,
    /// `/tms/1.0.0/{layer}/` — `<TileMap>` document for one layer.
    TileMapResource { layer: String },
    /// `/tms/1.0.0/{layer}/{z}/{x}/{y}.{ext}` — actual tile request.
    ///
    /// NOTE: TMS uses a **bottom-up Y axis**, i.e. `y=0` is the *southernmost*
    /// tile row. This parser preserves `y` as written on the wire — it does
    /// NOT flip to the XYZ/north-down convention. The route handler is
    /// responsible for flipping when it dispatches to the tile engine
    /// (`flipped_y = (1 << z) - 1 - y` for a standard quad pyramid).
    GetTile {
        layer: String,
        z: u32,
        x: u32,
        y: u32,
        format: OutputFormat,
    },
}

/// Parse a TMS path. The `/tms` prefix is assumed to have been stripped by
/// the route handler, so accepted shapes are:
///
/// * `""` or `"/"` → [`TmsRequest::Root`]
/// * `"/1.0.0"` or `"/1.0.0/"` → [`TmsRequest::ServiceMetadata`]
/// * `"/1.0.0/{layer}"` or `"/1.0.0/{layer}/"` → [`TmsRequest::TileMapResource`]
/// * `"/1.0.0/{layer}/{z}/{x}/{y}.{ext}"` → [`TmsRequest::GetTile`]
pub fn parse_path(path: &str) -> Result<TmsRequest> {
    // Normalize: trim a leading and trailing `/`. Empty after trim → Root.
    let trimmed = path.strip_prefix('/').unwrap_or(path);
    let trimmed = trimmed.strip_suffix('/').unwrap_or(trimmed);

    if trimmed.is_empty() {
        return Ok(TmsRequest::Root);
    }

    let segments: Vec<&str> = trimmed.split('/').collect();

    // First segment is always the version (we only speak 1.0.0).
    if segments[0] != "1.0.0" {
        bail!("TMS: unsupported version `{}` (expected `1.0.0`)", segments[0]);
    }

    match segments.len() {
        1 => Ok(TmsRequest::ServiceMetadata),
        2 => Ok(TmsRequest::TileMapResource {
            layer: segments[1].to_string(),
        }),
        5 => {
            let layer = segments[1].to_string();
            let z: u32 = segments[2]
                .parse()
                .with_context(|| format!("TMS: bad zoom `{}`", segments[2]))?;
            let x: u32 = segments[3]
                .parse()
                .with_context(|| format!("TMS: bad column `{}`", segments[3]))?;
            let last = segments[4];
            let (y_str, ext) = last
                .rsplit_once('.')
                .ok_or_else(|| anyhow!("TMS: final segment `{}` missing `.ext`", last))?;
            let y: u32 = y_str
                .parse()
                .with_context(|| format!("TMS: bad row `{}`", y_str))?;
            let format = format_from_ext(ext)?;
            Ok(TmsRequest::GetTile { layer, z, x, y, format })
        }
        _ => bail!(
            "TMS: unsupported path shape with {} segments (`{}`)",
            segments.len(),
            path
        ),
    }
}

fn format_from_ext(ext: &str) -> Result<OutputFormat> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Ok(OutputFormat::Png),
        "jpeg" | "jpg" => Ok(OutputFormat::Jpeg),
        "webp" => Ok(OutputFormat::Webp),
        "tif" | "tiff" => Ok(OutputFormat::GeoTiff),
        other => bail!("TMS: unsupported tile extension `{}`", other),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_root() {
        assert_eq!(parse_path("").unwrap(), TmsRequest::Root);
        assert_eq!(parse_path("/").unwrap(), TmsRequest::Root);
    }

    #[test]
    fn parse_service_metadata() {
        assert_eq!(parse_path("/1.0.0").unwrap(), TmsRequest::ServiceMetadata);
        assert_eq!(parse_path("/1.0.0/").unwrap(), TmsRequest::ServiceMetadata);
    }

    #[test]
    fn parse_tile_map_resource() {
        let req = parse_path("/1.0.0/ortho2024").unwrap();
        assert_eq!(
            req,
            TmsRequest::TileMapResource {
                layer: "ortho2024".into()
            }
        );
        let req2 = parse_path("/1.0.0/ortho2024/").unwrap();
        assert_eq!(req, req2);
    }

    #[test]
    fn parse_get_tile_preserves_y_no_flip() {
        let req = parse_path("/1.0.0/ortho2024/10/512/600.png").unwrap();
        match req {
            TmsRequest::GetTile { layer, z, x, y, format } => {
                assert_eq!(layer, "ortho2024");
                assert_eq!(z, 10);
                assert_eq!(x, 512);
                // The TMS bottom-up `y` is preserved exactly as-is: no flip.
                assert_eq!(y, 600);
                assert_eq!(format, OutputFormat::Png);
            }
            other => panic!("expected GetTile, got {other:?}"),
        }
    }

    #[test]
    fn parse_get_tile_each_extension() {
        for (ext, fmt) in [
            ("png", OutputFormat::Png),
            ("jpeg", OutputFormat::Jpeg),
            ("webp", OutputFormat::Webp),
            ("tif", OutputFormat::GeoTiff),
        ] {
            let path = format!("/1.0.0/l/0/0/0.{ext}");
            match parse_path(&path).expect("parses") {
                TmsRequest::GetTile { format, .. } => assert_eq!(format, fmt),
                other => panic!("expected GetTile, got {other:?}"),
            }
        }
    }

    #[test]
    fn rejects_bad_version() {
        let err = parse_path("/0.9.0/foo").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("1.0.0") || msg.to_lowercase().contains("version"));
    }

    #[test]
    fn rejects_malformed_tile_segment() {
        assert!(parse_path("/1.0.0/layer/0/0/no_ext").is_err());
        assert!(parse_path("/1.0.0/layer/a/0/0.png").is_err());
        assert!(parse_path("/1.0.0/a/b/c").is_err());
    }
}
