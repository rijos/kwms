//! WMTS 1.0.0 capabilities — XML generation.
//!
//! Single entry point: [`generate`]. Produces the full `<Capabilities>`
//! document (XML declaration + root) as a `String`.
//!
//! The output shape mirrors PDOK's reference WMTS capabilities document at
//! <https://service.pdok.nl/hwh/luchtfotorgb/wmts/v1_0?REQUEST=GetCapabilities>.
//!
//! Naming: well-known grids surface under their canonical WMTS TileMatrixSet
//! identifiers (e.g. `webmercator` → `GoogleMapsCompatible`); custom grids
//! keep their config key as-is.

use std::collections::BTreeMap;

use anyhow::{anyhow, Context, Result};
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

use crate::config::{Config, LayerConfig};
use crate::crs::{meters_per_unit, native_axis_order, AxisOrder};
use crate::tile::coord::TileCoord;
use crate::tile::grid::{lookup_grid, tile_matrix_set_name, TileGrid, OGC_PIXEL_SIZE_M};

/// Generate a WMTS 1.0.0 `<Capabilities>` document.
///
/// `base_url` is the absolute origin + path prefix used to build the
/// `<ResourceURL>` templates — e.g. `"https://example.org"`. The handler
/// supplies this from the inbound request.
pub fn generate(config: &Config, base_url: &str) -> Result<String> {
    // Pre-resolve all grids referenced by any layer so we can emit a single
    // `<TileMatrixSet>` block per unique grid even when several layers share
    // it. Use BTreeMap so output is stable across runs.
    let mut grids: BTreeMap<String, std::sync::Arc<dyn TileGrid>> = BTreeMap::new();
    for layer in &config.layers {
        for grid_name in &layer.grids {
            let g = lookup_grid(grid_name, &config.grids).ok_or_else(|| {
                anyhow!(
                    "WMTS capabilities: layer `{}` references unknown grid `{}`",
                    layer.name,
                    grid_name
                )
            })?;
            let tms_name = tile_matrix_set_name(grid_name).to_string();
            grids.entry(tms_name).or_insert(g);
        }
    }

    let mut writer = Writer::new(Vec::<u8>::new());
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .context("write XML declaration")?;

    // ---- root ----
    let mut root = BytesStart::new("Capabilities");
    root.push_attribute(("xmlns", "http://www.opengis.net/wmts/1.0"));
    root.push_attribute(("xmlns:ows", "http://www.opengis.net/ows/1.1"));
    root.push_attribute(("xmlns:xlink", "http://www.w3.org/1999/xlink"));
    root.push_attribute(("xmlns:xsi", "http://www.w3.org/2001/XMLSchema-instance"));
    root.push_attribute(("xmlns:gml", "http://www.opengis.net/gml"));
    root.push_attribute(("version", "1.0.0"));
    writer.write_event(Event::Start(root)).context("start root")?;

    // ---- ServiceIdentification ----
    write_service_identification(&mut writer)?;
    // ---- ServiceProvider ----
    write_service_provider(&mut writer)?;

    // ---- Contents ----
    writer
        .write_event(Event::Start(BytesStart::new("Contents")))
        .context("start Contents")?;

    for layer in &config.layers {
        write_layer(&mut writer, layer, base_url)?;
    }

    for (tms_name, grid) in &grids {
        write_tile_matrix_set(&mut writer, tms_name, grid.as_ref())?;
    }

    writer
        .write_event(Event::End(BytesEnd::new("Contents")))
        .context("end Contents")?;
    writer
        .write_event(Event::End(BytesEnd::new("Capabilities")))
        .context("end root")?;

    let bytes = writer.into_inner();
    Ok(String::from_utf8(bytes).context("WMTS capabilities not valid UTF-8")?)
}

// ---------------------------------------------------------------------------
// Section writers
// ---------------------------------------------------------------------------

fn write_service_identification<W: std::io::Write>(writer: &mut Writer<W>) -> Result<()> {
    writer
        .write_event(Event::Start(BytesStart::new("ows:ServiceIdentification")))
        .context("start ServiceIdentification")?;
    write_text(writer, "ows:Title", "kwms WMTS")?;
    write_text(writer, "ows:Abstract", "kwms — WMTS 1.0.0 service")?;
    write_text(writer, "ows:ServiceType", "OGC WMTS")?;
    write_text(writer, "ows:ServiceTypeVersion", "1.0.0")?;
    writer
        .write_event(Event::End(BytesEnd::new("ows:ServiceIdentification")))
        .context("end ServiceIdentification")?;
    Ok(())
}

fn write_service_provider<W: std::io::Write>(writer: &mut Writer<W>) -> Result<()> {
    writer
        .write_event(Event::Start(BytesStart::new("ows:ServiceProvider")))
        .context("start ServiceProvider")?;
    write_text(writer, "ows:ProviderName", "kwms")?;
    writer
        .write_event(Event::End(BytesEnd::new("ows:ServiceProvider")))
        .context("end ServiceProvider")?;
    Ok(())
}

fn write_layer<W: std::io::Write>(
    writer: &mut Writer<W>,
    layer: &LayerConfig,
    base_url: &str,
) -> Result<()> {
    writer
        .write_event(Event::Start(BytesStart::new("Layer")))
        .context("start Layer")?;

    write_text(writer, "ows:Title", &layer.title)?;
    write_text(writer, "ows:Abstract", &layer.abstract_)?;

    if !layer.keywords.is_empty() {
        writer
            .write_event(Event::Start(BytesStart::new("ows:Keywords")))
            .context("start Keywords")?;
        for kw in &layer.keywords {
            write_text(writer, "ows:Keyword", kw)?;
        }
        writer
            .write_event(Event::End(BytesEnd::new("ows:Keywords")))
            .context("end Keywords")?;
    }

    write_text(writer, "ows:Identifier", &layer.name)?;

    if let Some(b) = &layer.bbox {
        writer
            .write_event(Event::Start(BytesStart::new("ows:WGS84BoundingBox")))
            .context("start WGS84BoundingBox")?;
        write_text(writer, "ows:LowerCorner", &format!("{} {}", b.minx, b.miny))?;
        write_text(writer, "ows:UpperCorner", &format!("{} {}", b.maxx, b.maxy))?;
        writer
            .write_event(Event::End(BytesEnd::new("ows:WGS84BoundingBox")))
            .context("end WGS84BoundingBox")?;
    }

    // ---- Style (always one default) ----
    let mut style_el = BytesStart::new("Style");
    style_el.push_attribute(("isDefault", "true"));
    writer
        .write_event(Event::Start(style_el))
        .context("start Style")?;
    write_text(writer, "ows:Identifier", "default")?;
    writer
        .write_event(Event::End(BytesEnd::new("Style")))
        .context("end Style")?;

    // ---- Format ----
    for fmt in &layer.formats {
        write_text(writer, "Format", fmt.mime_type())?;
    }

    // ---- TileMatrixSetLink ----
    for grid_name in &layer.grids {
        let tms_name = tile_matrix_set_name(grid_name);
        writer
            .write_event(Event::Start(BytesStart::new("TileMatrixSetLink")))
            .context("start TileMatrixSetLink")?;
        write_text(writer, "TileMatrixSet", tms_name)?;
        writer
            .write_event(Event::End(BytesEnd::new("TileMatrixSetLink")))
            .context("end TileMatrixSetLink")?;
    }

    // ---- ResourceURL templates (one per format) ----
    for fmt in &layer.formats {
        // Same grammar `request::parse_rest` accepts — one definition.
        let template = format!(
            "{base}/wmts/{layer}/{tile}.{ext}",
            base = base_url.trim_end_matches('/'),
            layer = layer.name,
            tile = super::request::REST_TILE_TEMPLATE,
            ext = fmt.extension(),
        );
        let mut el = BytesStart::new("ResourceURL");
        el.push_attribute(("format", fmt.mime_type()));
        el.push_attribute(("resourceType", "tile"));
        el.push_attribute(("template", template.as_str()));
        writer
            .write_event(Event::Empty(el))
            .context("write ResourceURL")?;
    }

    writer
        .write_event(Event::End(BytesEnd::new("Layer")))
        .context("end Layer")?;
    Ok(())
}

fn write_tile_matrix_set<W: std::io::Write>(
    writer: &mut Writer<W>,
    tms_name: &str,
    grid: &dyn TileGrid,
) -> Result<()> {
    writer
        .write_event(Event::Start(BytesStart::new("TileMatrixSet")))
        .context("start TileMatrixSet")?;

    write_text(writer, "ows:Identifier", tms_name)?;
    write_text(writer, "ows:SupportedCRS", &supported_crs_urn(grid.crs().as_str()))?;

    let (tile_w, tile_h) = grid.tile_size();
    let top_left = grid.tile_bbox(TileCoord::new(0, 0, 0));
    // TopLeftCorner is written in the axis order of the SupportedCRS URN,
    // e.g. "lat lon" for urn:ogc:def:crs:EPSG::4326 and "N E" for ::3035.
    let corner = match native_axis_order(grid.crs()).context("TileMatrixSet CRS axis order")? {
        AxisOrder::XY => format!("{} {}", top_left.minx, top_left.maxy),
        AxisOrder::YX => format!("{} {}", top_left.maxy, top_left.minx),
    };
    // ScaleDenominator = resolution in metres / 0.28 mm (OGC standard pixel);
    // degree-based CRSes convert via the ellipsoid (GDAL), not assumed metres.
    let metres_per_unit = meters_per_unit(grid.crs()).context("TileMatrixSet CRS units")?;

    let levels = grid.level_count();
    for level in 0..levels {
        let res = grid.resolution(level);
        let scale_denom = res * metres_per_unit / OGC_PIXEL_SIZE_M;
        let (matrix_width, matrix_height) = grid.matrix_size(level);

        writer
            .write_event(Event::Start(BytesStart::new("TileMatrix")))
            .context("start TileMatrix")?;
        write_text(writer, "ows:Identifier", &level.to_string())?;
        write_text(writer, "ScaleDenominator", &format!("{scale_denom}"))?;
        write_text(writer, "TopLeftCorner", &corner)?;
        write_text(writer, "TileWidth", &tile_w.to_string())?;
        write_text(writer, "TileHeight", &tile_h.to_string())?;
        write_text(writer, "MatrixWidth", &matrix_width.to_string())?;
        write_text(writer, "MatrixHeight", &matrix_height.to_string())?;
        writer
            .write_event(Event::End(BytesEnd::new("TileMatrix")))
            .context("end TileMatrix")?;
    }

    writer
        .write_event(Event::End(BytesEnd::new("TileMatrixSet")))
        .context("end TileMatrixSet")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert an `EPSG:NNNN`-style identifier into the URN form WMTS prefers
/// in `<ows:SupportedCRS>` (`urn:ogc:def:crs:EPSG::NNNN`). Anything that's
/// not an `EPSG:` code passes through unchanged.
fn supported_crs_urn(crs: &str) -> String {
    if let Some(code) = crs.strip_prefix("EPSG:").or_else(|| crs.strip_prefix("epsg:")) {
        format!("urn:ogc:def:crs:EPSG::{code}")
    } else {
        crs.to_string()
    }
}

fn write_text<W: std::io::Write>(
    writer: &mut Writer<W>,
    tag: &str,
    text: &str,
) -> Result<()> {
    writer
        .write_event(Event::Start(BytesStart::new(tag)))
        .with_context(|| format!("start <{tag}>"))?;
    writer
        .write_event(Event::Text(BytesText::new(text)))
        .with_context(|| format!("text for <{tag}>"))?;
    writer
        .write_event(Event::End(BytesEnd::new(tag)))
        .with_context(|| format!("end <{tag}>"))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        Config, LayerCacheConfig, LayerConfig, MaxResolution, SourceConfig,
    };
    use crate::crs::Bbox;
    use crate::encode::OutputFormat;

    fn minimal_layer() -> LayerConfig {
        LayerConfig {
            name: "ortho2024".into(),
            grids: vec!["webmercator".into()],
            max_resolution: MaxResolution::default(),
            bbox: Some(Bbox::from([3.17, 50.58, 7.49, 53.64])),
            sources: vec![SourceConfig {
                uri: "vsiaz://photos/x.vrt".into(),
                credentials: None,
            }],
            cache: LayerCacheConfig::default(),
            title: "Luchtfoto 2024".into(),
            abstract_: "Test layer".into(),
            keywords: vec!["aerial".into(), "ortho".into()],
            attribution: None,
            metadata_url: None,
            formats: vec![OutputFormat::Png],
            resampling: "bilinear".into(),
            queryable: true,
            opaque: true,
            nodata: None,
        }
    }

    fn minimal_config() -> Config {
        Config {
            layers: vec![minimal_layer()],
            ..Default::default()
        }
    }

    #[test]
    fn contains_layer_identifier_and_tms_name() {
        let xml = generate(&minimal_config(), "https://example.org").unwrap();
        assert!(
            xml.contains("<ows:Identifier>ortho2024</ows:Identifier>"),
            "missing layer identifier; got:\n{xml}"
        );
        assert!(
            xml.contains("<TileMatrixSet>GoogleMapsCompatible</TileMatrixSet>"),
            "missing TMS link; got:\n{xml}"
        );
        assert!(
            xml.contains("<ows:Identifier>GoogleMapsCompatible</ows:Identifier>"),
            "missing TMS definition identifier; got:\n{xml}"
        );
    }

    #[test]
    fn contains_tile_matrix_blocks() {
        let xml = generate(&minimal_config(), "https://example.org").unwrap();
        assert!(xml.contains("<TileMatrix>"), "expected <TileMatrix> block");
        // Level 0 identifier inside a TileMatrix.
        assert!(xml.contains("<ows:Identifier>0</ows:Identifier>"));
        // Tile dims and matrix dims appear.
        assert!(xml.contains("<TileWidth>256</TileWidth>"));
        assert!(xml.contains("<MatrixWidth>1</MatrixWidth>"));
    }

    #[test]
    fn scale_denominator_nonzero() {
        let xml = generate(&minimal_config(), "https://example.org").unwrap();
        // Find a `<ScaleDenominator>...</ScaleDenominator>` and parse it.
        let start = xml.find("<ScaleDenominator>").expect("has ScaleDenominator");
        let after = &xml[start + "<ScaleDenominator>".len()..];
        let end = after.find("</ScaleDenominator>").expect("close tag");
        let value: f64 = after[..end].parse().expect("scale denom parses as f64");
        assert!(value > 0.0, "scale denominator should be > 0, got {value}");
    }

    #[test]
    fn contains_resource_url_template() {
        let xml = generate(&minimal_config(), "https://example.org").unwrap();
        assert!(
            xml.contains("<ResourceURL"),
            "expected ResourceURL element; got:\n{xml}"
        );
        assert!(
            xml.contains("resourceType=\"tile\""),
            "expected resourceType=tile on ResourceURL"
        );
        assert!(
            xml.contains(
                "https://example.org/wmts/ortho2024/{TileMatrixSet}/{TileMatrix}/{TileCol}/{TileRow}.png"
            ),
            "expected template substitution placeholders; got:\n{xml}"
        );
    }

    #[test]
    fn supported_crs_is_urn() {
        let xml = generate(&minimal_config(), "https://example.org").unwrap();
        assert!(
            xml.contains("<ows:SupportedCRS>urn:ogc:def:crs:EPSG::3857</ows:SupportedCRS>"),
            "expected URN-form SupportedCRS; got:\n{xml}"
        );
    }

    #[test]
    fn global_geodetic_doubles_width() {
        let mut layer = minimal_layer();
        layer.grids = vec!["global_geodetic".into()];
        let cfg = Config {
            layers: vec![layer],
            ..Default::default()
        };
        let xml = generate(&cfg, "https://example.org").unwrap();
        // WorldCRS84Quad has 2 tiles east-west at level 0.
        assert!(
            xml.contains("<MatrixWidth>2</MatrixWidth>"),
            "expected MatrixWidth=2 for WorldCRS84Quad level 0; got:\n{xml}"
        );
        assert!(
            xml.contains("<MatrixHeight>1</MatrixHeight>"),
            "expected MatrixHeight=1 for WorldCRS84Quad level 0; got:\n{xml}"
        );
    }

    #[test]
    fn geographic_matrix_set_uses_ogc_scale_and_lat_lon_corner() {
        let mut layer = minimal_layer();
        layer.grids = vec!["global_geodetic".into()];
        let cfg = Config { layers: vec![layer], ..Default::default() };
        let xml = generate(&cfg, "https://example.org").unwrap();
        assert!(xml.contains("<ows:Identifier>WorldCRS84Quad</ows:Identifier>"), "{xml}");
        // OGC WorldCRS84Quad level 0: 279541132.0143589 (degrees → metres
        // via the WGS84 equator), not 0.703125 / 0.00028 = 2511.16.
        let start = xml.find("<ScaleDenominator>").unwrap() + "<ScaleDenominator>".len();
        let end = start + xml[start..].find('<').unwrap();
        let scale: f64 = xml[start..end].parse().unwrap();
        assert!((scale - 279_541_132.014).abs() < 1.0, "scale {scale}");
        // EPSG:4326 is lat/lon: the corner is "90 -180".
        assert!(xml.contains("<TopLeftCorner>90 -180</TopLeftCorner>"), "{xml}");
    }

    #[test]
    fn projected_matrix_set_keeps_metre_scale_and_xy_corner() {
        let xml = generate(&minimal_config(), "https://example.org").unwrap();
        assert!(xml.contains("<ScaleDenominator>559082264.0287"), "{xml}");
        assert!(xml.contains("<TopLeftCorner>-20037508.3427892 20037508.3427892</TopLeftCorner>"));
    }
}
