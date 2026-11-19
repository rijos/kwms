//! Shared harness for the request fuzzers (`fuzz/`) and property tests.
//!
//! Compiled only for tests and fuzzing builds (`cfg(fuzzing)` is set by
//! cargo-fuzz). Each `check_*` function feeds untrusted input through a
//! protocol's stage 1 + stage 2 exactly as the server does, and asserts the
//! invariants that make the result safe to hand to the rest of the server:
//!
//! * nothing panics, for any input;
//! * a rejection renders, in every protocol's error format, as a
//!   well-formed exception document made only of printable ASCII — error
//!   content comes solely from our literals, so no client byte (control
//!   characters, bidi marks, markup, long values) can appear in it;
//! * an accepted request only refers to catalog entries — configured
//!   layers, grids and CRS tokens, never a client string — and all numeric
//!   fields are inside their limits.
//!
//! A failed assertion is a bug in the protocol layer.

use std::sync::OnceLock;

use quick_xml::events::Event;
use quick_xml::Reader;

use super::catalog::Catalog;
use super::error::OgcError;
use super::kvp::{Kvp, MAX_PARAMS};
use super::query::TileQuery;
use super::wms::request::WmsVersion;
use super::{tms, wms, wmts};
use crate::config::Config;

/// The catalog every check runs against: layers on all built-in grids
/// (easting-first and northing-first CRSes), plus a bottom-left custom grid.
pub fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let config: Config = serde_yaml::from_str(
            r#"
grids:
  local_bl:
    crs: EPSG:28992
    top_left: [0, 0]
    base_resolution: 4096
    origin: bottom_left
layers:
  - {name: ortho, title: Ortho, grids: [rd_new_quad, webmercator, global_geodetic], sources: [{uri: "memory://ortho"}], formats: [jpeg, png]}
  - {name: laea, title: LAEA, grids: [etrs89_laea_quad], sources: [{uri: "memory://laea"}], formats: [png]}
  - {name: local, title: Local, grids: [local_bl], sources: [{uri: "memory://local"}], formats: [png]}
"#,
        )
        .expect("harness config parses");
        crate::config::validate::validate(&config).expect("harness config is valid");
        Catalog::build(&config).expect("harness catalog builds")
    })
}

/// `Kvp::parse` on arbitrary input.
pub fn check_kvp(query: &str) {
    match Kvp::parse(query) {
        Ok(kvp) => {
            // Re-parsing the decoded pairs is not meaningful; check limits.
            let _ = kvp.get(super::error::Param::Request);
            assert!(query.len() <= super::kvp::MAX_QUERY_BYTES);
            assert!(query.split('&').filter(|p| !p.is_empty()).count() <= MAX_PARAMS);
        }
        Err(e) => check_error(&e),
    }
}

/// WMS stage 1 + stage 2 on an arbitrary query string.
pub fn check_wms(query: &str) {
    let cat = catalog();
    let req = match wms::request::parse(query) {
        Ok(r) => r,
        Err(e) => return check_error(&e),
    };
    match wms::validate::validate(req, cat) {
        Err(e) => check_error(&e),
        Ok(wms::validate::WmsQuery::GetCapabilities { .. }) => {}
        Ok(wms::validate::WmsQuery::GetMap(m)) => {
            let layer = cat.layer(m.layer().name()).expect("layer is a catalog entry");
            assert!(std::ptr::eq(layer, m.layer()));
            assert!(layer.grids().iter().any(|g| std::ptr::eq(g, m.grid())));
            // The CRS handed downstream is the grid's catalog CRS: a config
            // token, never the client's spelling.
            assert!(std::ptr::eq(cat.crs(m.grid().crs()), m.crs()));
            assert_eq!(m.crs().crs(), m.grid().grid().crs());
            let b = m.bbox();
            assert!([b.minx, b.miny, b.maxx, b.maxy].iter().all(|v| v.is_finite()));
            assert!(b.minx < b.maxx && b.miny < b.maxy);
            let (w, h) = m.size();
            let [max_w, max_h] = cat.max_request_pixels();
            assert!((1..=max_w).contains(&w) && (1..=max_h).contains(&h));
            assert!(m.layer().offers(m.format()));
        }
    }
}

/// WMTS KVP binding.
pub fn check_wmts_kvp(query: &str) {
    check_wmts(wmts::request::parse_kvp(query));
}

/// WMTS RESTful binding (the path below `/wmts/`).
pub fn check_wmts_rest(path: &str) {
    check_wmts(wmts::request::parse_rest(path));
}

fn check_wmts(parsed: Result<wmts::request::WmtsRequest, OgcError>) {
    let req = match parsed {
        Ok(r) => r,
        Err(e) => return check_error(&e),
    };
    match wmts::validate::validate(req, catalog()) {
        Err(e) => check_error(&e),
        Ok(wmts::validate::WmtsQuery::GetCapabilities) => {}
        Ok(wmts::validate::WmtsQuery::GetTile(t)) => check_tile(&t),
    }
}

/// TMS (the path below `/tms`).
pub fn check_tms(path: &str) {
    let req = match tms::request::parse_path(path) {
        Ok(r) => r,
        Err(e) => return check_error(&e),
    };
    match tms::validate::validate(req, catalog()) {
        Err(e) => check_error(&e),
        Ok(tms::validate::TmsQuery::GetTile(t)) => check_tile(&t),
        Ok(tms::validate::TmsQuery::TileMap(layer)) => {
            assert!(std::ptr::eq(catalog().layer(layer.name()).unwrap(), layer));
        }
        Ok(_) => {}
    }
}

fn check_tile(t: &TileQuery<'_>) {
    let cat = catalog();
    let layer = cat.layer(t.layer().name()).expect("layer is a catalog entry");
    assert!(std::ptr::eq(layer, t.layer()));
    assert!(layer.grids().iter().any(|g| std::ptr::eq(g, t.grid())));
    let g = t.grid().grid();
    let c = t.coord();
    assert!(c.z < g.level_count());
    let (cols, rows) = g.matrix_size(c.z);
    assert!(u64::from(c.x) < cols && u64::from(c.y) < rows);
    assert!(g.check_coord(c).is_ok());
    assert!(layer.offers(t.format()));
}

/// Every rejection must render as well-formed, printable-ASCII XML in every
/// protocol's error format.
fn check_error(e: &OgcError) {
    assert!(!e.message.is_empty());
    assert!(
        e.message.bytes().all(|b| (0x20..0x7f).contains(&b)),
        "error message is not printable ASCII: {:?}",
        e.message
    );
    for version in [WmsVersion::V1_1_1, WmsVersion::V1_3_0] {
        assert_well_formed(&wms::exception::for_error(e, version));
    }
    assert_well_formed(&wmts::exception::for_error(e).1);
    assert_well_formed(&tms::exception::for_error(e).1);
}

fn assert_well_formed(xml: &str) {
    // quick-xml's reader tolerates characters XML 1.0 forbids, so check the
    // bytes ourselves: printable ASCII plus the newline our writers emit.
    assert!(
        xml.bytes().all(|b| (0x20..0x7f).contains(&b) || b == b'\n'),
        "exception XML contains non-printable or non-ASCII bytes:\n{xml:?}"
    );
    let mut reader = Reader::from_str(xml);
    let mut depth = 0i32;
    loop {
        match reader.read_event() {
            Ok(Event::Start(_)) => depth += 1,
            Ok(Event::End(_)) => depth -= 1,
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(err) => panic!("malformed exception XML ({err}):\n{xml}"),
        }
    }
    assert_eq!(depth, 0, "unbalanced exception XML:\n{xml}");
}

#[cfg(test)]
mod tests {
    //! Property tests: the fuzz invariants on stable Rust, every `cargo test`.

    use proptest::prelude::*;

    use super::*;

    /// Query-string fragments that get deep into the validators.
    fn token() -> impl Strategy<Value = String> {
        prop_oneof![
            Just("SERVICE=WMS".to_string()),
            Just("SERVICE=WMTS".to_string()),
            Just("REQUEST=GetMap".to_string()),
            Just("REQUEST=GetTile".to_string()),
            Just("REQUEST=GetCapabilities".to_string()),
            Just("REQUEST=GetFeatureInfo".to_string()),
            Just("VERSION=1.3.0".to_string()),
            Just("VERSION=1.1.1".to_string()),
            Just("VERSION=1.0.0".to_string()),
            prop::sample::select(vec!["ortho", "laea", "local", "nope", ""]).prop_map(|l| format!("LAYERS={l}")),
            prop::sample::select(vec!["ortho", "laea", "local"]).prop_map(|l| format!("LAYER={l}")),
            prop::sample::select(vec!["", "default", "fancy", ",", "default,default"])
                .prop_map(|s| format!("STYLES={s}")),
            prop::sample::select(vec![
                "EPSG:3857",
                "EPSG:28992",
                "EPSG:4326",
                "EPSG:3035",
                "epsg:03857",
                "CRS:84",
                "urn:ogc:def:crs:EPSG::4326",
                "http://www.opengis.net/def/crs/EPSG/0/3035",
                "/etc/passwd",
                "http://169.254.169.254/",
                "EPSG%3A3857",
                "",
            ])
            .prop_flat_map(|c| prop::sample::select(vec!["CRS", "SRS"]).prop_map(move |k| format!("{k}={c}"))),
            (any::<[f64; 4]>()).prop_map(|b| format!("BBOX={},{},{},{}", b[0], b[1], b[2], b[3])),
            (-1e7f64..1e7, -1e7f64..1e7, 1f64..1e6, 1f64..1e6).prop_map(|(x, y, w, h)| format!(
                "BBOX={x},{y},{},{}",
                x + w,
                y + h
            )),
            (0u32..5000).prop_map(|w| format!("WIDTH={w}")),
            (0u32..5000).prop_map(|h| format!("HEIGHT={h}")),
            prop::sample::select(vec![
                "image/png",
                "image/jpeg",
                "image%2Fjpeg",
                "image/gif",
                "image/tiff"
            ])
            .prop_map(|f| format!("FORMAT={f}")),
            prop::sample::select(vec![
                "GoogleMapsCompatible",
                "NetherlandsRDNewQuad",
                "WorldCRS84Quad",
                "GoogleCRS84Quad",
                "EuropeanETRS89_LAEAQuad",
                "local_bl",
                "webmercator"
            ])
            .prop_map(|t| format!("TILEMATRIXSET={t}")),
            (0u32..30).prop_map(|z| format!("TILEMATRIX={z:02}")),
            (0u32..5000).prop_map(|r| format!("TILEROW={r}")),
            (0u32..5000).prop_map(|c| format!("TILECOL={c}")),
            "[a-zA-Z%=&+0-9:._/-]{0,24}",
        ]
    }

    fn query() -> impl Strategy<Value = String> {
        prop::collection::vec(token(), 0..14).prop_map(|t| t.join("&"))
    }

    /// A complete GetMap (every required parameter present) with each value
    /// drawn from valid and invalid choices, so a good share is accepted
    /// and the accepted-path invariants actually run.
    fn get_map_query() -> impl Strategy<Value = String> {
        // Weighted toward valid values (`ortho` offers every CRS listed
        // first, both formats), with invalid ones mixed in.
        (
            prop::sample::select(vec!["1.3.0", "1.1.1"]),
            prop::sample::select(vec!["ortho", "ortho", "ortho", "ortho", "laea", "local", "nope"]),
            prop::sample::select(vec!["", "", "default", "fancy"]),
            prop::sample::select(vec![
                "EPSG:3857",
                "EPSG:28992",
                "EPSG:4326",
                "urn:ogc:def:crs:EPSG::4326",
                "epsg:3857",
                "EPSG:3857",
                "EPSG:28992",
                "EPSG:4326",
                "EPSG:3035",
                "CRS:84",
                "/etc/passwd",
            ]),
            (-2e7f64..2e7, -2e7f64..2e7, 1e-3f64..5e6, 1e-3f64..5e6),
            (1u32..2700, 1u32..2700),
            prop::sample::select(vec!["image/png", "image/jpeg"]),
        )
            .prop_map(|(v, l, s, c, (x, y, w, h), (pw, ph), f)| {
                let k = if v == "1.1.1" { "SRS" } else { "CRS" };
                format!(
                    "SERVICE=WMS&VERSION={v}&REQUEST=GetMap&LAYERS={l}&STYLES={s}&{k}={c}\
                     &BBOX={x},{y},{},{}&WIDTH={pw}&HEIGHT={ph}&FORMAT={f}",
                    x + w,
                    y + h
                )
            })
    }

    /// A level and an index that is usually inside that level's matrix
    /// (power-of-two quad grids), sometimes just outside, sometimes junk.
    fn level_and_indices() -> impl Strategy<Value = (String, String, String)> {
        (0u32..26).prop_flat_map(|z| {
            let n = 1u64 << z.min(20);
            let idx = prop_oneof![
                8 => (0..n).prop_map(|v| v.to_string()),
                1 => (n..n + 4).prop_map(|v| v.to_string()),
                1 => "[0-9a-z+-]{0,4}",
            ];
            (Just(format!("{z:02}")), idx.clone(), idx)
        })
    }

    fn tile_path() -> impl Strategy<Value = String> {
        (
            prop::sample::select(vec!["ortho", "ortho", "local", "laea", "nope", ""]),
            prop::sample::select(vec![
                "GoogleMapsCompatible",
                "NetherlandsRDNewQuad",
                "local_bl",
                "EuropeanETRS89_LAEAQuad",
                "WorldCRS84Quad",
                "x",
            ]),
            level_and_indices(),
            prop::sample::select(vec!["png", "png", "jpeg", "jpg", "tif", "gif", ""]),
        )
            .prop_map(|(l, t, (z, c, r), e)| format!("{l}/{t}/{z}/{c}/{r}.{e}"))
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(512))]

        #[test]
        fn arbitrary_strings_never_break_any_protocol(s in ".{0,300}") {
            check_kvp(&s);
            check_wms(&s);
            check_wmts_kvp(&s);
            check_wmts_rest(&s);
            check_tms(&s);
        }

        #[test]
        fn structured_queries_hold_invariants(q in query()) {
            check_kvp(&q);
            check_wms(&q);
            check_wmts_kvp(&q);
        }

        #[test]
        fn complete_get_maps_hold_invariants(q in get_map_query()) {
            check_wms(&q);
        }

        #[test]
        fn structured_tile_paths_hold_invariants(p in tile_path()) {
            check_wmts_rest(&p);
            check_tms(&format!("/1.0.0/{}", p.split_once('/').map_or("", |x| x.1)));
            check_tms(&format!("/1.0.0/{p}"));
        }
    }

    #[test]
    fn generators_reach_the_accepted_path() {
        // A property test that only ever sees rejections proves little:
        // require that a fair share of generated requests is accepted.
        use proptest::strategy::ValueTree;
        use proptest::test_runner::TestRunner;
        let cat = catalog();
        let mut runner = TestRunner::deterministic();
        let (mut maps, mut tiles) = (0, 0);
        for _ in 0..500 {
            let q = get_map_query().new_tree(&mut runner).unwrap().current();
            if matches!(
                wms::request::parse(&q).and_then(|r| wms::validate::validate(r, cat)),
                Ok(wms::validate::WmsQuery::GetMap(_))
            ) {
                maps += 1;
            }
            let p = tile_path().new_tree(&mut runner).unwrap().current();
            if wmts::request::parse_rest(&p)
                .and_then(|r| wmts::validate::validate(r, cat))
                .is_ok()
            {
                tiles += 1;
            }
        }
        assert!(maps >= 100, "only {maps}/500 generated GetMaps were accepted");
        assert!(tiles >= 10, "only {tiles}/500 generated tile paths were accepted");
    }

    #[test]
    fn hostile_bytes_never_reach_exception_documents() {
        // The audit's reproducers: control chars, U+FFFE, bidi, long paths.
        let long = "'".repeat(60_000);
        for q in [
            "SERVICE=WMS&REQUEST=GetCapabilities&VERSION=%00%01%1B%5B31m",
            "SERVICE=WMS&REQUEST=GetCapabilities&VERSION=%EF%BF%BE",
            "SERVICE=WMS&REQUEST=GetCapabilities&VERSION=%E2%80%AEevil%E2%80%8B",
            "a%01b=1&A%01B=2",
            "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=%3Cx%3E&STYLES=&CRS=EPSG:3857&BBOX=0,0,1,1&WIDTH=1&HEIGHT=1&FORMAT=image/png",
        ] {
            check_wms(q);
            check_wmts_kvp(q);
        }
        for p in [
            "%01/t/0/0/0.png",
            "\u{1}/GoogleMapsCompatible/0/0/0.png",
            &format!("{long}/t/0/0/0.png"),
        ] {
            check_wmts_rest(p);
        }
        check_tms("/1.0.0/\u{1}");
        check_tms(&format!("/1.0.0/{long}/0/0/0.png"));
    }

    #[test]
    fn harness_accepts_known_good_requests() {
        // Guards against a harness that only ever sees rejections.
        let cat = catalog();
        let ok = |q: &str| {
            matches!(
                wms::request::parse(q).and_then(|r| wms::validate::validate(r, cat)),
                Ok(wms::validate::WmsQuery::GetMap(_))
            )
        };
        assert!(ok("SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=ortho&STYLES=&CRS=EPSG:4326&BBOX=50,3,54,7&WIDTH=256&HEIGHT=256&FORMAT=image/jpeg"));
        assert!(wmts::request::parse_rest("local/local_bl/3/1/2.png")
            .and_then(|r| wmts::validate::validate(r, cat))
            .is_ok());
    }
}
