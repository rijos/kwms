//! WMS GetCapabilities XML generation.
//!
//! Builds either a `<WMT_MS_Capabilities>` (1.1.1) or `<WMS_Capabilities>`
//! (1.3.0) document from a kwms [`Config`]. The two flavors share most of
//! their structure; the version-specific bits are:
//!
//! | Concern                | 1.1.1                       | 1.3.0                            |
//! |------------------------|-----------------------------|----------------------------------|
//! | Root element           | `<WMT_MS_Capabilities>`     | `<WMS_Capabilities>`             |
//! | CRS element name       | `<SRS>`                     | `<CRS>`                          |
//! | WGS84 bbox element     | `<LatLonBoundingBox>`       | `<EX_GeographicBoundingBox>`     |
//! | Axis order on BBOX     | always XY                   | YX for geographic-YX CRSes       |
//! | OGC namespace          | omitted                     | declared on root                 |
//!
//! Everything else (`<Service>`, `<Capability><Request>`, per-layer
//! `<BoundingBox>`s) is shared.

use anyhow::{Context, Result};

use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

use crate::config::{Config, LayerConfig};
use crate::crs::{transform_bbox, Bbox, Crs};
use crate::encode::OutputFormat;
use crate::tile::grid::lookup_grid;

use super::axis_order::should_swap_axes;
use super::request::WmsVersion;

/// Default WGS84 bbox used when a layer doesn't declare one in config.
const GLOBAL_WGS84_BBOX: Bbox = Bbox {
    minx: -180.0,
    miny: -90.0,
    maxx: 180.0,
    maxy: 90.0,
};

/// Build a GetCapabilities XML document for the given config and version.
///
/// `base_url` is used to populate `OnlineResource` `xlink:href` attributes
/// (the URL clients use to submit follow-up requests). Callers typically
/// pass the absolute URL of the WMS endpoint without a trailing query
/// string, e.g. `https://example.com/wms`.
pub fn generate(config: &Config, version: WmsVersion, base_url: &str) -> Result<String> {
    let mut buf: Vec<u8> = Vec::with_capacity(8 * 1024);
    let mut w = Writer::new_with_indent(&mut buf, b' ', 2);

    w.write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))?;

    let root_name = root_element_name(version);
    let mut root = BytesStart::new(root_name);
    root.push_attribute(("version", version.as_str()));
    if matches!(version, WmsVersion::V1_3_0) {
        // 1.3.0 declares both the WMS namespace and the xlink namespace
        // (used for `OnlineResource@xlink:href`). 1.1.1 used unprefixed
        // attributes for the same purpose, so we omit xmlns there.
        root.push_attribute(("xmlns", "http://www.opengis.net/wms"));
        root.push_attribute(("xmlns:xlink", "http://www.w3.org/1999/xlink"));
    } else {
        root.push_attribute(("xmlns:xlink", "http://www.w3.org/1999/xlink"));
    }
    w.write_event(Event::Start(root))?;

    write_service(&mut w, config, base_url)?;
    write_capability(&mut w, config, version, base_url)?;

    w.write_event(Event::End(BytesEnd::new(root_name)))?;

    Ok(String::from_utf8(buf).expect("quick-xml emits valid UTF-8"))
}

fn root_element_name(version: WmsVersion) -> &'static str {
    match version {
        WmsVersion::V1_1_1 => "WMT_MS_Capabilities",
        WmsVersion::V1_3_0 => "WMS_Capabilities",
    }
}

fn crs_element_name(version: WmsVersion) -> &'static str {
    match version {
        WmsVersion::V1_1_1 => "SRS",
        WmsVersion::V1_3_0 => "CRS",
    }
}

// ---------------------------------------------------------------------------
// <Service> — server-level metadata
// ---------------------------------------------------------------------------

fn write_service<W: std::io::Write>(w: &mut Writer<W>, _config: &Config, base_url: &str) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("Service")))?;

    write_text_element(w, "Name", "WMS")?;
    // We don't yet have a top-level `service.title` config field; emit a
    // sensible static default. Layer metadata is what clients actually
    // browse anyway.
    write_text_element(w, "Title", "kwms WMS")?;
    write_text_element(
        w,
        "Abstract",
        "WMS endpoint served by kwms — a Rust WMS/WMTS/TMS server.",
    )?;
    write_online_resource(w, base_url)?;

    w.write_event(Event::End(BytesEnd::new("Service")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// <Capability> — request catalog + layer tree
// ---------------------------------------------------------------------------

fn write_capability<W: std::io::Write>(
    w: &mut Writer<W>,
    config: &Config,
    version: WmsVersion,
    base_url: &str,
) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("Capability")))?;

    write_request_block(w, config, version, base_url)?;
    write_exception_block(w, version)?;

    for layer in &config.layers {
        write_layer(w, layer, config, version)?;
    }

    w.write_event(Event::End(BytesEnd::new("Capability")))?;
    Ok(())
}

fn write_request_block<W: std::io::Write>(
    w: &mut Writer<W>,
    config: &Config,
    version: WmsVersion,
    base_url: &str,
) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("Request")))?;

    // GetCapabilities — always `text/xml`. (1.3.0 actually mandates
    // `text/xml`; 1.1.1 used `application/vnd.ogc.wms_xml` historically
    // but `text/xml` is universally accepted, so we advertise the modern
    // form for both.)
    write_request_op(w, "GetCapabilities", &["text/xml"], base_url)?;

    // GetMap — union of all formats advertised by any layer, preserving
    // first-occurrence order so clients see a stable list.
    let map_formats = collect_layer_formats(config);
    let map_format_strs: Vec<&str> = map_formats.iter().map(|f| f.mime_type()).collect();
    write_request_op(w, "GetMap", &map_format_strs, base_url)?;

    // GetFeatureInfo — only meaningful if at least one layer is queryable,
    // but we always advertise it so clients can probe. Plain text + JSON
    // cover the common consumers; XML is included for legacy GIS clients.
    let _ = version; // version doesn't affect the format set we advertise here
    write_request_op(
        w,
        "GetFeatureInfo",
        &["text/plain", "application/json", "text/xml"],
        base_url,
    )?;

    w.write_event(Event::End(BytesEnd::new("Request")))?;
    Ok(())
}

fn write_request_op<W: std::io::Write>(w: &mut Writer<W>, op: &str, formats: &[&str], base_url: &str) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new(op)))?;
    for fmt in formats {
        write_text_element(w, "Format", fmt)?;
    }
    // DCPType / HTTP / Get — both versions use the same shape.
    w.write_event(Event::Start(BytesStart::new("DCPType")))?;
    w.write_event(Event::Start(BytesStart::new("HTTP")))?;
    w.write_event(Event::Start(BytesStart::new("Get")))?;
    write_online_resource(w, base_url)?;
    w.write_event(Event::End(BytesEnd::new("Get")))?;
    w.write_event(Event::End(BytesEnd::new("HTTP")))?;
    w.write_event(Event::End(BytesEnd::new("DCPType")))?;
    w.write_event(Event::End(BytesEnd::new(op)))?;
    Ok(())
}

fn write_exception_block<W: std::io::Write>(w: &mut Writer<W>, version: WmsVersion) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new("Exception")))?;
    // 1.3.0 mandates `XML`; 1.1.1 historically allowed `application/vnd.ogc.se_xml`.
    let token = match version {
        WmsVersion::V1_1_1 => "application/vnd.ogc.se_xml",
        WmsVersion::V1_3_0 => "XML",
    };
    write_text_element(w, "Format", token)?;
    w.write_event(Event::End(BytesEnd::new("Exception")))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// <Layer> — per-layer block
// ---------------------------------------------------------------------------

fn write_layer<W: std::io::Write>(
    w: &mut Writer<W>,
    layer: &LayerConfig,
    config: &Config,
    version: WmsVersion,
) -> Result<()> {
    let mut start = BytesStart::new("Layer");
    start.push_attribute(("queryable", if layer.queryable { "1" } else { "0" }));
    start.push_attribute(("opaque", if layer.opaque { "1" } else { "0" }));
    w.write_event(Event::Start(start))?;

    write_text_element(w, "Name", &layer.name)?;
    write_text_element(w, "Title", &layer.title)?;
    if !layer.abstract_.is_empty() {
        write_text_element(w, "Abstract", &layer.abstract_)?;
    }

    if !layer.keywords.is_empty() {
        w.write_event(Event::Start(BytesStart::new("KeywordList")))?;
        for kw in &layer.keywords {
            write_text_element(w, "Keyword", kw)?;
        }
        w.write_event(Event::End(BytesEnd::new("KeywordList")))?;
    }

    // CRS / SRS — union over each grid's CRS, deduplicated, in
    // first-occurrence order. We deliberately advertise the grid CRSes
    // (and *only* those); reprojection-on-the-fly to other CRSes is a
    // future extension and would belong on this list when it lands.
    let crses = collect_layer_crses(layer, config);
    let crs_tag = crs_element_name(version);
    for crs in &crses {
        write_text_element(w, crs_tag, crs.as_str())?;
    }

    // Geographic bbox — element name differs per version, but the
    // numbers (always WGS84) are the same.
    let wgs84 = Crs::new("EPSG:4326");
    let wgs84_bbox = crate::protocol::layer_bbox_in(layer, &wgs84)?.unwrap_or(GLOBAL_WGS84_BBOX);
    write_geographic_bbox(w, &wgs84_bbox, version)?;

    // Per-CRS BoundingBox blocks. Transform the WGS84 bbox into each
    // advertised CRS and respect the version's axis order on output.
    for crs in &crses {
        // From the configured bbox directly (exact in its own CRS); built
        // once at startup: a CRS GDAL/PROJ can't transform into is a
        // configuration error, reported — never a silently missing box.
        let target_bbox = match crate::protocol::layer_bbox_in(layer, crs)? {
            Some(b) => b,
            None if crs.as_str().eq_ignore_ascii_case("EPSG:4326") || crs.as_str().eq_ignore_ascii_case("CRS:84") => {
                wgs84_bbox
            }
            None => transform_bbox(&wgs84_bbox, &wgs84, crs)
                .with_context(|| format!("BoundingBox of layer `{}` in {crs}", layer.name))?,
        };
        write_bounding_box(w, crs, &target_bbox, version)?;
    }

    // Attribution.
    if let Some(attr) = &layer.attribution {
        w.write_event(Event::Start(BytesStart::new("Attribution")))?;
        write_text_element(w, "Title", attr)?;
        w.write_event(Event::End(BytesEnd::new("Attribution")))?;
    }

    // MetadataURL — 1.3.0 requires a type attribute; we advertise "ISO19115:2003".
    if let Some(url) = &layer.metadata_url {
        let mut mds = BytesStart::new("MetadataURL");
        mds.push_attribute(("type", "ISO19115:2003"));
        w.write_event(Event::Start(mds))?;
        write_text_element(w, "Format", "text/xml")?;
        write_online_resource(w, url)?;
        w.write_event(Event::End(BytesEnd::new("MetadataURL")))?;
    }

    // Style — kwms doesn't expose multiple styles yet; advertise a
    // single "default" so clients can reference it explicitly.
    w.write_event(Event::Start(BytesStart::new("Style")))?;
    write_text_element(w, "Name", "default")?;
    write_text_element(w, "Title", "Default style")?;
    w.write_event(Event::End(BytesEnd::new("Style")))?;

    w.write_event(Event::End(BytesEnd::new("Layer")))?;
    Ok(())
}

fn write_geographic_bbox<W: std::io::Write>(w: &mut Writer<W>, bbox: &Bbox, version: WmsVersion) -> Result<()> {
    match version {
        WmsVersion::V1_1_1 => {
            // <LatLonBoundingBox minx miny maxx maxy /> — note that
            // despite the *name*, the attributes are numeric XY: the
            // spec defines `minx`/`maxx` as longitude and `miny`/`maxy`
            // as latitude.
            let mut el = BytesStart::new("LatLonBoundingBox");
            el.push_attribute(("minx", fmt_f64(bbox.minx).as_str()));
            el.push_attribute(("miny", fmt_f64(bbox.miny).as_str()));
            el.push_attribute(("maxx", fmt_f64(bbox.maxx).as_str()));
            el.push_attribute(("maxy", fmt_f64(bbox.maxy).as_str()));
            w.write_event(Event::Empty(el))?;
        }
        WmsVersion::V1_3_0 => {
            // <EX_GeographicBoundingBox> with explicit west/east/south/north
            // sub-elements. No axis-order ambiguity here.
            w.write_event(Event::Start(BytesStart::new("EX_GeographicBoundingBox")))?;
            write_text_element(w, "westBoundLongitude", &fmt_f64(bbox.minx))?;
            write_text_element(w, "eastBoundLongitude", &fmt_f64(bbox.maxx))?;
            write_text_element(w, "southBoundLatitude", &fmt_f64(bbox.miny))?;
            write_text_element(w, "northBoundLatitude", &fmt_f64(bbox.maxy))?;
            w.write_event(Event::End(BytesEnd::new("EX_GeographicBoundingBox")))?;
        }
    }
    Ok(())
}

fn write_bounding_box<W: std::io::Write>(w: &mut Writer<W>, crs: &Crs, bbox: &Bbox, version: WmsVersion) -> Result<()> {
    let crs_attr = crs_element_name(version);

    // 1.3.0: for geographic-YX CRSes, emit attributes in YX order
    // (minimum-latitude first), matching the wire-format BBOX convention.
    // A CRS whose axis order GDAL can't report is an error, never a guess:
    // a wrong order would advertise a transposed BoundingBox.
    let swap = should_swap_axes(crs, version).with_context(|| format!("axis order of advertised CRS '{crs}'"))?;

    let mut el = BytesStart::new("BoundingBox");
    el.push_attribute((crs_attr, crs.as_str()));
    if swap {
        el.push_attribute(("minx", fmt_f64(bbox.miny).as_str()));
        el.push_attribute(("miny", fmt_f64(bbox.minx).as_str()));
        el.push_attribute(("maxx", fmt_f64(bbox.maxy).as_str()));
        el.push_attribute(("maxy", fmt_f64(bbox.maxx).as_str()));
    } else {
        el.push_attribute(("minx", fmt_f64(bbox.minx).as_str()));
        el.push_attribute(("miny", fmt_f64(bbox.miny).as_str()));
        el.push_attribute(("maxx", fmt_f64(bbox.maxx).as_str()));
        el.push_attribute(("maxy", fmt_f64(bbox.maxy).as_str()));
    }
    w.write_event(Event::Empty(el))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn collect_layer_crses(layer: &LayerConfig, config: &Config) -> Vec<Crs> {
    let mut out: Vec<Crs> = Vec::new();
    for grid_name in &layer.grids {
        if let Some(grid) = lookup_grid(grid_name, &config.grids) {
            let crs = grid.crs().clone();
            if !out.iter().any(|c| c == &crs) {
                out.push(crs);
            }
        }
    }
    out
}

fn collect_layer_formats(config: &Config) -> Vec<OutputFormat> {
    let mut out: Vec<OutputFormat> = Vec::new();
    for layer in &config.layers {
        for f in &layer.formats {
            if !out.contains(f) {
                out.push(*f);
            }
        }
    }
    // Fall back to PNG if no layers / no formats were configured — the
    // capabilities doc still has to advertise *something*.
    if out.is_empty() {
        out.push(OutputFormat::Png);
    }
    out
}

fn write_text_element<W: std::io::Write>(w: &mut Writer<W>, name: &str, text: &str) -> Result<()> {
    w.write_event(Event::Start(BytesStart::new(name)))?;
    w.write_event(Event::Text(BytesText::new(text)))?;
    w.write_event(Event::End(BytesEnd::new(name)))?;
    Ok(())
}

fn write_online_resource<W: std::io::Write>(w: &mut Writer<W>, href: &str) -> Result<()> {
    let mut el = BytesStart::new("OnlineResource");
    el.push_attribute(("xmlns:xlink", "http://www.w3.org/1999/xlink"));
    el.push_attribute(("xlink:type", "simple"));
    el.push_attribute(("xlink:href", href));
    w.write_event(Event::Empty(el))?;
    Ok(())
}

/// Render an `f64` as a stable, non-scientific string suitable for an XML
/// attribute. `{:.10}` keeps enough precision for sub-meter accuracy on
/// global extents without spraying scientific notation.
fn fmt_f64(v: f64) -> String {
    // Trim trailing zeros (and a trailing decimal point) so that round
    // numbers don't end up as `180.0000000000`.
    let mut s = format!("{:.10}", v);
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{LayerCacheConfig, LayerConfig, MaxResolution, SourceConfig};
    use crate::crs::Bbox;
    use crate::encode::OutputFormat;

    fn minimal_config() -> Config {
        Config {
            layers: vec![LayerConfig {
                name: "ortho2024".into(),
                grids: vec!["webmercator".into()],
                max_resolution: MaxResolution::default(),
                bbox_crs: None,
                bbox: Some(Bbox {
                    minx: 3.0,
                    miny: 50.0,
                    maxx: 7.0,
                    maxy: 54.0,
                }),
                sources: vec![SourceConfig {
                    uri: "test.tif".into(),
                    credentials: None,
                    ..Default::default()
                }],
                cache: LayerCacheConfig::default(),
                title: "Ortho 2024".into(),
                abstract_: "Test layer.".into(),
                keywords: vec!["test".into(), "ortho".into()],
                attribution: None,
                metadata_url: None,
                formats: vec![OutputFormat::Png],
                resampling: "bilinear".into(),
                queryable: true,
                opaque: false,
                nodata: None,
                source_mask: None,
                output_formats: Default::default(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn generates_111_with_correct_root_and_srs() {
        let cfg = minimal_config();
        let xml = generate(&cfg, WmsVersion::V1_1_1, "http://example.com/wms").unwrap();

        assert!(
            xml.contains("<WMT_MS_Capabilities"),
            "missing 1.1.1 root element: {xml}"
        );
        assert!(!xml.contains("<WMS_Capabilities"), "wrong root for 1.1.1: {xml}");
        assert!(xml.contains("version=\"1.1.1\""), "missing version attr");

        // Layer name is present.
        assert!(xml.contains("<Name>ortho2024</Name>"));
        // 1.1.1 uses <SRS>, not <CRS>.
        assert!(
            xml.contains("<SRS>EPSG:3857</SRS>"),
            "expected <SRS>EPSG:3857</SRS>: {xml}"
        );
        assert!(!xml.contains("<CRS>EPSG:3857</CRS>"));
        // Per-CRS BoundingBox element present.
        assert!(xml.contains("<BoundingBox"), "no <BoundingBox> in: {xml}");
        // 1.1.1 lat/lon bbox.
        assert!(xml.contains("<LatLonBoundingBox"));
        assert!(!xml.contains("<EX_GeographicBoundingBox"));
    }

    #[test]
    fn generates_130_with_correct_root_and_crs() {
        let cfg = minimal_config();
        let xml = generate(&cfg, WmsVersion::V1_3_0, "http://example.com/wms").unwrap();

        assert!(xml.contains("<WMS_Capabilities"), "missing 1.3.0 root: {xml}");
        assert!(!xml.contains("<WMT_MS_Capabilities"), "wrong root for 1.3.0: {xml}");
        assert!(xml.contains("version=\"1.3.0\""));
        // 1.3.0 namespace is bound on root.
        assert!(xml.contains("xmlns=\"http://www.opengis.net/wms\""));

        assert!(xml.contains("<Name>ortho2024</Name>"));
        assert!(
            xml.contains("<CRS>EPSG:3857</CRS>"),
            "expected <CRS>EPSG:3857</CRS>: {xml}"
        );
        assert!(!xml.contains("<SRS>EPSG:3857</SRS>"));
        assert!(xml.contains("<BoundingBox"), "no <BoundingBox> in: {xml}");
        // 1.3.0 uses EX_GeographicBoundingBox instead of LatLonBoundingBox.
        assert!(xml.contains("<EX_GeographicBoundingBox"));
        assert!(!xml.contains("<LatLonBoundingBox"));
    }

    #[test]
    fn request_block_advertises_layer_formats() {
        let cfg = minimal_config();
        let xml = generate(&cfg, WmsVersion::V1_3_0, "http://example.com/wms").unwrap();
        // GetMap should list the layer's PNG format.
        assert!(
            xml.contains("<Format>image/png</Format>"),
            "missing image/png GetMap format: {xml}"
        );
        // GetCapabilities advertises text/xml.
        assert!(xml.contains("<Format>text/xml</Format>"));
    }
}
