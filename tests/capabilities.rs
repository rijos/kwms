//! Pdok-XML tests for GetCapabilities across the four protocol flavors:
//! WMS 1.1.1, WMS 1.3.0, WMTS 1.0.0, and TMS 1.0.0.
//!
//! For each fixture under `tests/fixtures/config/`, we render the four
//! documents and diff (after whitespace normalization) against a pdok
//! XML file under `tests/fixtures/pdok/capabilities/`.
//!
//! Bootstrap workflow: pdok files are checked into the repo. To accept
//! new output (e.g. after a deliberate format change), set
//! `UPDATE_PDOK=1` and re-run; the failing assertion is replaced
//! by a file write, and the regenerated file gets committed after review.

mod common;

use kwms::protocol::tms::capabilities as tms;
use kwms::protocol::wms::capabilities as wms;
use kwms::protocol::wms::request::WmsVersion;
use kwms::protocol::wmts::capabilities as wmts;

const BASE_URL: &str = "https://example.org";

/// The fixture set this binary asserts pdok for. Same list as
/// `tests/config_merge.rs::FIXTURES` — kept in lockstep with the actual
/// files under `tests/fixtures/config/`.
const FIXTURES: &[&str] = &[
    "single_grid_minimal",
    "pdok_multigrid",
    "max_resolution_capped",
    "flags_off",
    "custom_grid",
];

// -------------------------------------------------------------------------
// WMS 1.1.1
// -------------------------------------------------------------------------

#[test]
fn wms_111_capabilities_match_pdok() {
    for fx in FIXTURES {
        let (config, _) = common::load_fixture(fx);
        let xml = wms::generate(&config, WmsVersion::V1_1_1, BASE_URL)
            .unwrap_or_else(|e| panic!("WMS 1.1.1 capabilities for `{fx}` failed: {e:#}"));
        common::assert_xml_matches_pdok(&xml, &format!("capabilities/wms_111_{fx}.xml"));
    }
}

// -------------------------------------------------------------------------
// WMS 1.3.0
// -------------------------------------------------------------------------

#[test]
fn wms_130_capabilities_match_pdok() {
    for fx in FIXTURES {
        let (config, _) = common::load_fixture(fx);
        let xml = wms::generate(&config, WmsVersion::V1_3_0, BASE_URL)
            .unwrap_or_else(|e| panic!("WMS 1.3.0 capabilities for `{fx}` failed: {e:#}"));
        common::assert_xml_matches_pdok(&xml, &format!("capabilities/wms_130_{fx}.xml"));
    }
}

// -------------------------------------------------------------------------
// WMTS 1.0.0
// -------------------------------------------------------------------------

#[test]
fn wmts_capabilities_match_pdok() {
    for fx in FIXTURES {
        let (config, _) = common::load_fixture(fx);
        let xml =
            wmts::generate(&config, BASE_URL).unwrap_or_else(|e| panic!("WMTS capabilities for `{fx}` failed: {e:#}"));
        common::assert_xml_matches_pdok(&xml, &format!("capabilities/wmts_{fx}.xml"));
    }
}

// -------------------------------------------------------------------------
// TMS 1.0.0
// -------------------------------------------------------------------------

#[test]
fn tms_root_document_matches_pdok() {
    // The TMS root document is the same for every fixture (it depends
    // only on `base_url`), so we assert it once and don't carry per-fixture
    // copies.
    let xml = tms::generate_root(BASE_URL);
    common::assert_xml_matches_pdok(&xml, "capabilities/tms_root.xml");
}

#[test]
fn tms_service_metadata_matches_pdok() {
    for fx in FIXTURES {
        let (config, _) = common::load_fixture(fx);
        let xml = tms::generate_service_metadata(&config, BASE_URL)
            .unwrap_or_else(|e| panic!("TMS service metadata for `{fx}` failed: {e:#}"));
        common::assert_xml_matches_pdok(&xml, &format!("capabilities/tms_service_{fx}.xml"));
    }
}

#[test]
fn tms_per_layer_tile_map_matches_pdok() {
    for fx in FIXTURES {
        let (config, _) = common::load_fixture(fx);
        for layer in &config.layers {
            let xml = tms::generate_tile_map(layer, &config, BASE_URL)
                .unwrap_or_else(|e| panic!("TMS TileMap for fixture `{fx}` layer `{}` failed: {e:#}", layer.name));
            common::assert_xml_matches_pdok(&xml, &format!("capabilities/tms_layer_{fx}_{}.xml", layer.name));
        }
    }
}

// -------------------------------------------------------------------------
// Shape / smoke assertions (independent of pdok files)
// -------------------------------------------------------------------------
//
// These run even without pdok files — they catch regressions in the
// fixture set (e.g. someone breaking a fixture in a way that still
// produces *some* XML).

#[test]
fn wms_111_uses_legacy_root_element() {
    let (config, _) = common::load_fixture("single_grid_minimal");
    let xml = wms::generate(&config, WmsVersion::V1_1_1, BASE_URL).unwrap();
    assert!(
        xml.contains("<WMT_MS_Capabilities"),
        "WMS 1.1.1 should use the <WMT_MS_Capabilities> root; got:\n{xml}"
    );
}

#[test]
fn wms_130_uses_new_root_element() {
    let (config, _) = common::load_fixture("single_grid_minimal");
    let xml = wms::generate(&config, WmsVersion::V1_3_0, BASE_URL).unwrap();
    assert!(
        xml.contains("<WMS_Capabilities"),
        "WMS 1.3.0 should use the <WMS_Capabilities> root; got:\n{xml}"
    );
}

#[test]
fn wmts_root_is_capabilities_element() {
    let (config, _) = common::load_fixture("single_grid_minimal");
    let xml = wmts::generate(&config, BASE_URL).unwrap();
    assert!(
        xml.contains("<Capabilities"),
        "WMTS should use the <Capabilities> root; got:\n{xml}"
    );
}

#[test]
fn flags_off_layer_queryable_attribute_reflects_config() {
    // The XML attribute name differs slightly across protocols, but every
    // one of them encodes the queryable flag somehow. We just check that
    // the attribute is observably present and reflects the toggled flag.
    let (config, _) = common::load_fixture("flags_off");
    let xml_111 = wms::generate(&config, WmsVersion::V1_1_1, BASE_URL).unwrap();
    let xml_130 = wms::generate(&config, WmsVersion::V1_3_0, BASE_URL).unwrap();
    assert!(
        xml_111.contains("queryable=\"0\"") || xml_111.contains("queryable=\"false\""),
        "WMS 1.1.1 should advertise queryable=false; got:\n{xml_111}"
    );
    assert!(
        xml_130.contains("queryable=\"0\"") || xml_130.contains("queryable=\"false\""),
        "WMS 1.3.0 should advertise queryable=false; got:\n{xml_130}"
    );
}

#[test]
fn pdok_multigrid_layer_advertises_each_grid() {
    // Per-CRS BoundingBox / TileMatrixSetLink: every grid in the layer's
    // grids: list should be reflected in the WMTS output.
    let (config, _) = common::load_fixture("pdok_multigrid");
    let xml = wmts::generate(&config, BASE_URL).unwrap();
    // PDOK multigrid uses webmercator + rd_new_quad, both well-known grids,
    // so WMTS publishes their canonical TileMatrixSet names (no `wmts:`
    // overrides in this fixture): `GoogleMapsCompatible` and
    // `NetherlandsRDNewQuad`. Each of the two layers links both, and each
    // set is defined once.
    for tms in ["GoogleMapsCompatible", "NetherlandsRDNewQuad"] {
        let link = format!("<TileMatrixSetLink><TileMatrixSet>{tms}</TileMatrixSet></TileMatrixSetLink>");
        assert_eq!(
            xml.matches(&link).count(),
            2,
            "{tms}: expected a link from each layer; got:\n{xml}"
        );
        let def = format!("<TileMatrixSet><ows:Identifier>{tms}</ows:Identifier>");
        assert_eq!(
            xml.matches(&def).count(),
            1,
            "{tms}: expected one definition; got:\n{xml}"
        );
    }
    assert!(
        !xml.contains("rd_new_quad"),
        "config grid names must not leak into WMTS; got:\n{xml}"
    );
}
