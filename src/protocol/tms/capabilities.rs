//! TMS 1.0.0 capabilities — three documents, one per nesting level.
//!
//! * [`generate_root`] — `/tms/` listing all TMS versions.
//! * [`generate_service_metadata`] — `/tms/1.0.0/` listing all layers.
//! * [`generate_tile_map`] — `/tms/1.0.0/{layer}/` describing one layer's
//!   tile pyramid in detail.

use anyhow::{anyhow, Context, Result};
use quick_xml::events::{BytesDecl, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::Writer;

use crate::config::{Config, LayerConfig};
use crate::crs::{transform_bbox, Crs};
use crate::tile::grid::{lookup_grid, tms_profile};

// ---------------------------------------------------------------------------
// /tms/ — root document
// ---------------------------------------------------------------------------

/// Generate the TMS root document (`/tms/`).
pub fn generate_root(base_url: &str) -> String {
    let base = base_url.trim_end_matches('/');
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<Services>
  <TileMapService title="kwms TMS" version="1.0.0" href="{base}/tms/1.0.0/"/>
</Services>"#
    )
}

// ---------------------------------------------------------------------------
// /tms/1.0.0/ — service metadata
// ---------------------------------------------------------------------------

/// Generate `/tms/1.0.0/` — the `<TileMapService>` document listing every
/// configured layer.
pub fn generate_service_metadata(config: &Config, base_url: &str) -> Result<String> {
    let base = base_url.trim_end_matches('/');

    let mut writer = Writer::new(Vec::<u8>::new());
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .context("write XML decl")?;

    let mut root = BytesStart::new("TileMapService");
    root.push_attribute(("version", "1.0.0"));
    root.push_attribute(("services", "."));
    writer
        .write_event(Event::Start(root))
        .context("start TileMapService")?;

    write_text(&mut writer, "Title", "kwms TMS")?;
    write_text(&mut writer, "Abstract", "kwms — TMS 1.0.0 service")?;

    writer
        .write_event(Event::Start(BytesStart::new("TileMaps")))
        .context("start TileMaps")?;

    for layer in &config.layers {
        // Pick the layer's first grid for the advertised `srs` attribute —
        // TMS predates the multi-CRS layer concept, so a layer-per-CRS view
        // is the simplest stable mapping.
        let grid_name = layer.grids.first().ok_or_else(|| {
            anyhow!(
                "TMS service metadata: layer `{}` has no grids configured",
                layer.name
            )
        })?;
        let grid = lookup_grid(grid_name, &config.grids).ok_or_else(|| {
            anyhow!(
                "TMS service metadata: layer `{}` references unknown grid `{}`",
                layer.name,
                grid_name
            )
        })?;

        let mut tm = BytesStart::new("TileMap");
        let href = format!("{base}/tms/1.0.0/{}", layer.name);
        let srs = grid.crs().as_str().to_string();
        let profile = tms_profile(grid_name).to_string();
        tm.push_attribute(("href", href.as_str()));
        tm.push_attribute(("srs", srs.as_str()));
        tm.push_attribute(("title", layer.title.as_str()));
        tm.push_attribute(("profile", profile.as_str()));
        writer
            .write_event(Event::Empty(tm))
            .context("write TileMap entry")?;
    }

    writer
        .write_event(Event::End(BytesEnd::new("TileMaps")))
        .context("end TileMaps")?;
    writer
        .write_event(Event::End(BytesEnd::new("TileMapService")))
        .context("end TileMapService")?;

    let bytes = writer.into_inner();
    Ok(String::from_utf8(bytes).context("TMS service metadata not valid UTF-8")?)
}

// ---------------------------------------------------------------------------
// /tms/1.0.0/{layer}/ — per-layer document
// ---------------------------------------------------------------------------

/// Generate the per-layer `<TileMap>` document.
pub fn generate_tile_map(
    layer: &LayerConfig,
    config: &Config,
    base_url: &str,
) -> Result<String> {
    let base = base_url.trim_end_matches('/');

    let grid_name = layer.grids.first().ok_or_else(|| {
        anyhow!("TMS TileMap: layer `{}` has no grids configured", layer.name)
    })?;
    let grid = lookup_grid(grid_name, &config.grids).ok_or_else(|| {
        anyhow!(
            "TMS TileMap: layer `{}` references unknown grid `{}`",
            layer.name,
            grid_name
        )
    })?;
    let fmt = layer.formats.first().ok_or_else(|| {
        anyhow!(
            "TMS TileMap: layer `{}` declares no output formats",
            layer.name
        )
    })?;

    let mut writer = Writer::new(Vec::<u8>::new());
    writer
        .write_event(Event::Decl(BytesDecl::new("1.0", Some("UTF-8"), None)))
        .context("write XML decl")?;

    let mut root = BytesStart::new("TileMap");
    root.push_attribute(("version", "1.0.0"));
    let tms_url = format!("{base}/tms/1.0.0/");
    root.push_attribute(("tilemapservice", tms_url.as_str()));
    writer
        .write_event(Event::Start(root))
        .context("start TileMap")?;

    write_text(&mut writer, "Title", &layer.title)?;
    write_text(&mut writer, "Abstract", &layer.abstract_)?;
    write_text(&mut writer, "SRS", grid.crs().as_str())?;

    // BoundingBox is in the TileMap's SRS (the grid CRS). `layer.bbox` is
    // WGS84, so reproject it (GDAL) — a failure is an error, not a silent
    // fallback; without a layer bbox, use the grid's extent.
    let bbox = match layer.bbox {
        Some(b) => transform_bbox(&b, &Crs::new("EPSG:4326"), grid.crs())
            .with_context(|| format!("BoundingBox of layer `{}` in {}", layer.name, grid.crs()))?,
        None => grid.extent(),
    };
    let mut bbox_el = BytesStart::new("BoundingBox");
    let (minx, miny, maxx, maxy) = (bbox.minx, bbox.miny, bbox.maxx, bbox.maxy);
    let minx_s = minx.to_string();
    let miny_s = miny.to_string();
    let maxx_s = maxx.to_string();
    let maxy_s = maxy.to_string();
    bbox_el.push_attribute(("minx", minx_s.as_str()));
    bbox_el.push_attribute(("miny", miny_s.as_str()));
    bbox_el.push_attribute(("maxx", maxx_s.as_str()));
    bbox_el.push_attribute(("maxy", maxy_s.as_str()));
    writer
        .write_event(Event::Empty(bbox_el))
        .context("write BoundingBox")?;

    // TMS addresses rows bottom-up, so its Origin is the lower-left corner
    // of the tile matrix (not the top-left that WMTS uses).
    let extent = grid.extent();
    let (ox, oy) = (extent.minx, extent.miny);
    let mut origin_el = BytesStart::new("Origin");
    let ox_s = ox.to_string();
    let oy_s = oy.to_string();
    origin_el.push_attribute(("x", ox_s.as_str()));
    origin_el.push_attribute(("y", oy_s.as_str()));
    writer
        .write_event(Event::Empty(origin_el))
        .context("write Origin")?;

    // TileFormat — width / height / mime / extension from the layer's first
    // declared output format. (TMS allows only one tile format per
    // `<TileMap>` document.)
    let (tile_w, tile_h) = grid.tile_size();
    let mut tf = BytesStart::new("TileFormat");
    let tw = tile_w.to_string();
    let th = tile_h.to_string();
    tf.push_attribute(("width", tw.as_str()));
    tf.push_attribute(("height", th.as_str()));
    tf.push_attribute(("mime-type", fmt.mime_type()));
    tf.push_attribute(("extension", fmt.extension()));
    writer
        .write_event(Event::Empty(tf))
        .context("write TileFormat")?;

    // TileSets — one TileSet per level.
    let profile = tms_profile(grid_name);
    let mut tilesets_el = BytesStart::new("TileSets");
    tilesets_el.push_attribute(("profile", profile));
    writer
        .write_event(Event::Start(tilesets_el))
        .context("start TileSets")?;

    let levels = grid.level_count();
    for level in 0..levels {
        let units_per_pixel = grid.resolution(level);
        let mut ts = BytesStart::new("TileSet");
        let href = format!("{base}/tms/1.0.0/{}/{}", layer.name, level);
        let upp = format!("{units_per_pixel}");
        let order = level.to_string();
        ts.push_attribute(("href", href.as_str()));
        ts.push_attribute(("units-per-pixel", upp.as_str()));
        ts.push_attribute(("order", order.as_str()));
        writer
            .write_event(Event::Empty(ts))
            .context("write TileSet")?;
    }

    writer
        .write_event(Event::End(BytesEnd::new("TileSets")))
        .context("end TileSets")?;
    writer
        .write_event(Event::End(BytesEnd::new("TileMap")))
        .context("end TileMap")?;

    let bytes = writer.into_inner();
    Ok(String::from_utf8(bytes).context("TMS TileMap not valid UTF-8")?)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

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
            keywords: vec![],
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
    fn root_contains_tile_map_service() {
        let xml = generate_root("https://example.org");
        assert!(
            xml.contains("<TileMapService"),
            "root missing <TileMapService>; got:\n{xml}"
        );
        assert!(
            xml.contains("href=\"https://example.org/tms/1.0.0/\""),
            "root href not set correctly; got:\n{xml}"
        );
    }

    #[test]
    fn service_metadata_lists_layers() {
        let xml =
            generate_service_metadata(&minimal_config(), "https://example.org").unwrap();
        assert!(
            xml.contains("<TileMapService"),
            "missing root element; got:\n{xml}"
        );
        assert!(
            xml.contains("href=\"https://example.org/tms/1.0.0/ortho2024\""),
            "layer href missing; got:\n{xml}"
        );
        assert!(
            xml.contains("srs=\"EPSG:3857\""),
            "expected layer srs EPSG:3857; got:\n{xml}"
        );
        assert!(
            xml.contains("title=\"Luchtfoto 2024\""),
            "expected layer title; got:\n{xml}"
        );
    }

    #[test]
    fn tile_map_has_tilesets_per_level() {
        let cfg = minimal_config();
        let xml =
            generate_tile_map(&cfg.layers[0], &cfg, "https://example.org").unwrap();
        assert!(
            xml.contains("<TileSets"),
            "missing <TileSets>; got:\n{xml}"
        );
        // WebMercator has 22 levels.
        let count = xml.matches("<TileSet ").count();
        assert_eq!(
            count, 22,
            "expected 22 TileSet entries (one per level); got {count}:\n{xml}"
        );
        // Each TileSet should carry an `order=` from 0 upwards.
        assert!(xml.contains("order=\"0\""), "expected order=0 TileSet");
        assert!(xml.contains("order=\"21\""), "expected order=21 TileSet");
        // SRS and origin propagate from the grid.
        assert!(xml.contains("<SRS>EPSG:3857</SRS>"));
        assert!(xml.contains("<Origin "), "missing <Origin> element");
        assert!(xml.contains("<BoundingBox "), "missing <BoundingBox>");
        // TileFormat reflects PNG.
        assert!(
            xml.contains("mime-type=\"image/png\""),
            "expected PNG mime-type"
        );
        assert!(
            xml.contains("extension=\"png\""),
            "expected png extension"
        );
    }
}
