//! WMTS 1.0.0 capabilities — XML generation.
//!
//! Single entry point: [`generate`]. Produces the full `<Capabilities>`
//! document (XML declaration + root) as a `String`.
//!
//! The output shape mirrors PDOK's reference WMTS capabilities document at
//! <https://service.pdok.nl/hwh/luchtfotorgb/wmts/v1_0?REQUEST=GetCapabilities>:
//! service metadata from `service:`, OperationsMetadata with KVP and
//! RESTful bindings, per-layer `ows:Metadata` links, and TileMatrixSet /
//! TileMatrix identifiers and `SupportedCRS` style from `wmts:`.
//!
//! Naming: a grid is published under the identifiers listed for it in
//! `wmts.tile_matrix_sets` (several are allowed), else its default — the
//! canonical WMTS name for well-known grids (`webmercator` →
//! `GoogleMapsCompatible`), the config key for custom grids.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use quick_xml::events::{BytesDecl, BytesStart, Event};
use quick_xml::Writer;

use crate::config::{Config, CrsIdentifierStyle, LayerConfig, WmtsConfig};
use crate::crs::{meters_per_unit, native_axis_order, AxisOrder};
use crate::protocol::ows::{
    end, start, write_href, write_operations_metadata, write_service_identification, write_service_provider,
    write_text, GetBinding,
};
use crate::tile::coord::TileCoord;
use crate::tile::grid::{lookup_grid, TileGrid, OGC_PIXEL_SIZE_M};

/// One published TileMatrixSet: its identifier and grid.
struct Published {
    id: String,
    grid_name: String,
    grid: Arc<dyn TileGrid>,
}

/// Generate a WMTS 1.0.0 `<Capabilities>` document.
///
/// `base_url` is the WMTS endpoint as clients reach it — public URL plus
/// `wmts.path`, e.g. `"https://service.example.org/hwh/luchtfotorgb/wmts/v1_0"`.
/// KVP operations are advertised at `{base_url}?`, RESTful ones below it.
pub fn generate(config: &Config, base_url: &str) -> Result<String> {
    let base = base_url.trim_end_matches('/');
    let published = published_sets(config)?;

    let mut writer = Writer::new(Vec::<u8>::new());
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .context("write XML declaration")?;

    let mut root = BytesStart::new("Capabilities");
    root.push_attribute(("xmlns", "http://www.opengis.net/wmts/1.0"));
    root.push_attribute(("xmlns:ows", "http://www.opengis.net/ows/1.1"));
    root.push_attribute(("xmlns:xlink", "http://www.w3.org/1999/xlink"));
    root.push_attribute(("xmlns:xsi", "http://www.w3.org/2001/XMLSchema-instance"));
    root.push_attribute(("xmlns:gml", "http://www.opengis.net/gml"));
    root.push_attribute((
        "xsi:schemaLocation",
        "http://www.opengis.net/wmts/1.0 http://schemas.opengis.net/wmts/1.0/wmtsGetCapabilities_response.xsd",
    ));
    root.push_attribute(("version", "1.0.0"));
    writer.write_event(Event::Start(root)).context("start root")?;

    write_service_identification(&mut writer, &config.service, "kwms WMTS", "OGC WMTS", "1.0.0")?;
    write_service_provider(&mut writer, &config.service)?;
    let kvp = format!("{base}?");
    write_operations_metadata(
        &mut writer,
        &["GetCapabilities", "GetTile"],
        &[
            GetBinding {
                href: &kvp,
                encoding: "KVP",
            },
            GetBinding {
                href: base,
                encoding: "RESTful",
            },
        ],
    )?;

    start(&mut writer, "Contents")?;
    for layer in &config.layers {
        write_layer(&mut writer, layer, &published, base)?;
    }
    for p in &published {
        write_tile_matrix_set(&mut writer, p, &config.wmts)?;
    }
    end(&mut writer, "Contents")?;
    write_href(
        &mut writer,
        "ServiceMetadataURL",
        &format!("{base}/{}", super::request::REST_CAPABILITIES_PATH),
    )?;
    end(&mut writer, "Capabilities")?;

    String::from_utf8(writer.into_inner()).context("WMTS capabilities not valid UTF-8")
}

/// Every TileMatrixSet to publish, in order: the `wmts.tile_matrix_sets`
/// entries for grids some layer uses, then (default names) the used grids
/// not listed there. Identifiers must be unique.
fn published_sets(config: &Config) -> Result<Vec<Published>> {
    let mut used: Vec<&String> = Vec::new();
    for layer in &config.layers {
        for g in &layer.grids {
            if !used.contains(&g) {
                used.push(g);
            }
        }
    }
    let lookup = |name: &str| {
        lookup_grid(name, &config.grids).ok_or_else(|| anyhow!("WMTS capabilities: unknown grid `{name}`"))
    };
    let mut out: Vec<Published> = Vec::new();
    for entry in &config.wmts.tile_matrix_sets {
        if used.contains(&&entry.grid) {
            out.push(Published {
                id: entry.identifier.clone(),
                grid_name: entry.grid.clone(),
                grid: lookup(&entry.grid)?,
            });
        }
    }
    for name in used {
        if !out.iter().any(|p| &p.grid_name == name) {
            for id in config.wmts.tile_matrix_set_ids(name) {
                out.push(Published {
                    id,
                    grid_name: name.clone(),
                    grid: lookup(name)?,
                });
            }
        }
    }
    let mut seen = HashSet::new();
    for p in &out {
        if !seen.insert(p.id.as_str()) {
            return Err(anyhow!("TileMatrixSet identifier `{}` is used twice", p.id));
        }
    }
    Ok(out)
}

fn write_layer<W: std::io::Write>(
    writer: &mut Writer<W>,
    layer: &LayerConfig,
    published: &[Published],
    base: &str,
) -> Result<()> {
    start(writer, "Layer")?;
    // OWS DatasetDescriptionSummary order: Title, Abstract, Keywords,
    // WGS84BoundingBox, Identifier, BoundingBox, Metadata.
    write_text(writer, "ows:Title", &layer.title)?;
    write_text(writer, "ows:Abstract", &layer.abstract_)?;
    if !layer.keywords.is_empty() {
        start(writer, "ows:Keywords")?;
        for kw in &layer.keywords {
            write_text(writer, "ows:Keyword", kw)?;
        }
        end(writer, "ows:Keywords")?;
    }
    if let Some(b) = &crate::protocol::layer_bbox_in(layer, &crate::crs::Crs::new("EPSG:4326"))? {
        start(writer, "ows:WGS84BoundingBox")?;
        write_text(writer, "ows:LowerCorner", &format!("{} {}", b.minx, b.miny))?;
        write_text(writer, "ows:UpperCorner", &format!("{} {}", b.maxx, b.maxy))?;
        end(writer, "ows:WGS84BoundingBox")?;
    }
    write_text(writer, "ows:Identifier", &layer.name)?;
    if let Some(url) = &layer.metadata_url {
        write_href(writer, "ows:Metadata", url)?;
    }

    let mut style_el = BytesStart::new("Style");
    style_el.push_attribute(("isDefault", "true"));
    writer.write_event(Event::Start(style_el)).context("start Style")?;
    write_text(writer, "ows:Identifier", "default")?;
    end(writer, "Style")?;

    for fmt in &layer.formats {
        write_text(writer, "Format", fmt.mime_type())?;
    }
    for p in published.iter().filter(|p| layer.grids.contains(&p.grid_name)) {
        start(writer, "TileMatrixSetLink")?;
        write_text(writer, "TileMatrixSet", &p.id)?;
        end(writer, "TileMatrixSetLink")?;
    }
    for fmt in &layer.formats {
        // Same grammar `request::parse_rest` accepts — one definition.
        let template = format!(
            "{base}/{layer}/{tile}.{ext}",
            layer = layer.name,
            tile = super::request::REST_TILE_TEMPLATE,
            ext = fmt.extension(),
        );
        let mut el = BytesStart::new("ResourceURL");
        el.push_attribute(("format", fmt.mime_type()));
        el.push_attribute(("resourceType", "tile"));
        el.push_attribute(("template", template.as_str()));
        writer.write_event(Event::Empty(el)).context("write ResourceURL")?;
    }
    end(writer, "Layer")
}

fn write_tile_matrix_set<W: std::io::Write>(writer: &mut Writer<W>, p: &Published, wmts: &WmtsConfig) -> Result<()> {
    let grid = p.grid.as_ref();
    start(writer, "TileMatrixSet")?;
    write_text(writer, "ows:Identifier", &p.id)?;
    write_text(
        writer,
        "ows:SupportedCRS",
        &supported_crs(grid.crs().as_str(), wmts.crs_identifier),
    )?;

    let (tile_w, tile_h) = grid.tile_size();
    let top_left = grid.tile_bbox(TileCoord::new(0, 0, 0));
    // TopLeftCorner is written in the CRS's axis order, e.g. "lat lon" for
    // EPSG:4326 / EPSG:4258 and "N E" for EPSG:3035.
    let corner = match native_axis_order(grid.crs()).context("TileMatrixSet CRS axis order")? {
        AxisOrder::XY => format!("{} {}", top_left.minx, top_left.maxy),
        AxisOrder::YX => format!("{} {}", top_left.maxy, top_left.minx),
    };
    // ScaleDenominator = resolution in metres / 0.28 mm (OGC standard pixel);
    // degree-based CRSes convert via the ellipsoid (GDAL), not assumed metres.
    let metres_per_unit = meters_per_unit(grid.crs()).context("TileMatrixSet CRS units")?;
    for level in 0..grid.level_count() {
        let scale_denom = grid.resolution(level) * metres_per_unit / OGC_PIXEL_SIZE_M;
        let (matrix_width, matrix_height) = grid.matrix_size(level);
        start(writer, "TileMatrix")?;
        write_text(writer, "ows:Identifier", &wmts.tile_matrix_id(level))?;
        write_text(writer, "ScaleDenominator", &format!("{scale_denom}"))?;
        write_text(writer, "TopLeftCorner", &corner)?;
        write_text(writer, "TileWidth", &tile_w.to_string())?;
        write_text(writer, "TileHeight", &tile_h.to_string())?;
        write_text(writer, "MatrixWidth", &matrix_width.to_string())?;
        write_text(writer, "MatrixHeight", &matrix_height.to_string())?;
        end(writer, "TileMatrix")?;
    }
    end(writer, "TileMatrixSet")
}

/// `ows:SupportedCRS` for an `EPSG:NNNN` CRS: the URN
/// (`urn:ogc:def:crs:EPSG::NNNN`) or the short form, per config. Anything
/// that's not an `EPSG:` code passes through unchanged.
fn supported_crs(crs: &str, style: CrsIdentifierStyle) -> String {
    match (crs.strip_prefix("EPSG:").or_else(|| crs.strip_prefix("epsg:")), style) {
        (Some(code), CrsIdentifierStyle::Urn) => format!("urn:ogc:def:crs:EPSG::{code}"),
        (Some(code), CrsIdentifierStyle::Epsg) => format!("EPSG:{code}"),
        (None, _) => crs.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, LayerCacheConfig, LayerConfig, MaxResolution, SourceConfig};
    use crate::crs::Bbox;
    use crate::encode::OutputFormat;

    fn minimal_layer() -> LayerConfig {
        LayerConfig {
            name: "ortho2024".into(),
            grids: vec!["webmercator".into()],
            max_resolution: MaxResolution::default(),
            bbox_crs: None,
            bbox: Some(Bbox::from([3.17, 50.58, 7.49, 53.64])),
            sources: vec![SourceConfig {
                uri: "vsiaz://photos/x.vrt".into(),
                credentials: None,
                ..Default::default()
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
            source_mask: None,
            output_formats: Default::default(),
            cache_control: None,
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
        let xml = generate(&minimal_config(), "https://example.org/wmts").unwrap();
        assert!(
            xml.contains("<ResourceURL"),
            "expected ResourceURL element; got:\n{xml}"
        );
        assert!(
            xml.contains("resourceType=\"tile\""),
            "expected resourceType=tile on ResourceURL"
        );
        assert!(
            xml.contains("https://example.org/wmts/ortho2024/{TileMatrixSet}/{TileMatrix}/{TileCol}/{TileRow}.png"),
            "expected template substitution placeholders; got:\n{xml}"
        );
    }

    /// PDOK-style publication through config: EPSG ids, two names for one
    /// grid, zero-padded TileMatrix ids, short CRS ids, service metadata,
    /// layer metadata link, OperationsMetadata and ServiceMetadataURL.
    #[test]
    fn pdok_style_publication() {
        let mut cfg = minimal_config();
        cfg.wmts.tile_matrix_digits = 2;
        cfg.wmts.crs_identifier = CrsIdentifierStyle::Epsg;
        cfg.wmts.tile_matrix_sets = vec![
            crate::config::TileMatrixSetName {
                grid: "webmercator".into(),
                identifier: "EPSG:3857".into(),
            },
            crate::config::TileMatrixSetName {
                grid: "webmercator".into(),
                identifier: "OGC:1.0:GoogleMapsCompatible".into(),
            },
        ];
        cfg.service.title = Some("Landelijke Voorziening Beeldmateriaal".into());
        cfg.service.fees = Some("none".into());
        cfg.layers[0].metadata_url = Some("https://example.org/md?uuid=1".into());
        let base = "https://service.example.org/hwh/luchtfotorgb/wmts/v1_0";
        let xml = generate(&cfg, base).unwrap();
        for needle in [
            "<ows:Title>Landelijke Voorziening Beeldmateriaal</ows:Title>",
            "<ows:Fees>none</ows:Fees>",
            "<ows:Get xlink:href=\"https://service.example.org/hwh/luchtfotorgb/wmts/v1_0?\">",
            "<ows:Value>RESTful</ows:Value>",
            "<ows:Identifier>ortho2024</ows:Identifier><ows:Metadata xlink:href=\"https://example.org/md?uuid=1\"/>",
            "<TileMatrixSetLink><TileMatrixSet>EPSG:3857</TileMatrixSet></TileMatrixSetLink>\
             <TileMatrixSetLink><TileMatrixSet>OGC:1.0:GoogleMapsCompatible</TileMatrixSet></TileMatrixSetLink>",
            "<ows:Identifier>EPSG:3857</ows:Identifier><ows:SupportedCRS>EPSG:3857</ows:SupportedCRS>",
            "<ows:Identifier>00</ows:Identifier>",
            "<ows:Identifier>21</ows:Identifier>",
            "template=\"https://service.example.org/hwh/luchtfotorgb/wmts/v1_0/ortho2024/{TileMatrixSet}/",
            "<ServiceMetadataURL xlink:href=\"https://service.example.org/hwh/luchtfotorgb/wmts/v1_0/1.0.0/WMTSCapabilities.xml\"/>",
        ] {
            assert!(xml.contains(needle), "missing {needle}\n{xml}");
        }
        assert!(
            !xml.contains("<TileMatrixSet>GoogleMapsCompatible</TileMatrixSet>"),
            "default name still advertised"
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
        let cfg = Config {
            layers: vec![layer],
            ..Default::default()
        };
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
