//! Integration tests for config loading, deep-merge, env-var overrides,
//! and JSON Schema export.
//!
//! Unit tests in `src/config/{load,merge,validate}.rs` already cover the
//! algorithmic side. This binary covers the **filesystem dimension**: real
//! fixture YAMLs under `tests/fixtures/config/` get parsed by `load()` and
//! validated end-to-end. A regression here means a fixture file is wrong
//! or `load()` broke for a real-world layout.

mod common;

use std::io::Write;
use std::sync::Mutex;

use kwms::config::schema::emit_json_schema;

// All fixture configs in `tests/fixtures/config/`. Keep in sync with the
// files in that directory — the all-fixtures test below iterates this list.
const FIXTURES: &[&str] = &[
    "single_grid_minimal",
    "pdok_multigrid",
    "max_resolution_capped",
    "flags_off",
    "custom_grid",
];

// Tests in this binary that mutate `KWMS_*` env vars must hold this mutex.
// Test threads in one binary share process state; the unit tests in
// src/config/load.rs use the same pattern.
static ENV_LOCK: Mutex<()> = Mutex::new(());

// -----------------------------------------------------------------------
// Fixture round-trip
// -----------------------------------------------------------------------

#[test]
fn every_fixture_loads_and_validates() {
    for fx in FIXTURES {
        let (config, secrets) = common::load_fixture(fx);
        assert!(
            !config.layers.is_empty(),
            "fixture `{fx}` has no layers — that's never valid for capabilities tests"
        );
        // Every layer must reference at least one source URI under our
        // fixture path convention.
        for layer in &config.layers {
            assert!(
                !layer.sources.is_empty(),
                "fixture `{fx}` layer `{}` has no sources",
                layer.name
            );
            for src in &layer.sources {
                assert!(
                    src.uri.starts_with("tests/fixtures/sources/"),
                    "fixture `{fx}` layer `{}` source `{}` does not point at \
                     tests/fixtures/sources/ — fixture-gen workflow assumes it does",
                    layer.name,
                    src.uri
                );
            }
        }
        // None of our fixtures declare credentials.
        assert!(
            secrets.credentials.is_empty(),
            "fixture `{fx}` unexpectedly declared credentials"
        );
    }
}

#[test]
fn custom_grid_fixture_declares_one_custom_grid() {
    let (config, _) = common::load_fixture("custom_grid");
    assert_eq!(
        config.grids.len(),
        1,
        "custom_grid fixture should declare exactly one custom grid"
    );
    assert!(
        config.grids.contains_key("rd_custom_local"),
        "expected custom grid `rd_custom_local`; got {:?}",
        config.grids.keys().collect::<Vec<_>>()
    );
}

#[test]
fn pdok_multigrid_fixture_advertises_multiple_grids_per_layer() {
    let (config, _) = common::load_fixture("pdok_multigrid");
    for layer in &config.layers {
        assert!(
            layer.grids.len() >= 2,
            "pdok_multigrid layer `{}` should advertise multiple grids; got {:?}",
            layer.name,
            layer.grids
        );
    }
}

#[test]
fn flags_off_fixture_toggles_both_flags() {
    let (config, _) = common::load_fixture("flags_off");
    let l = &config.layers[0];
    assert!(!l.queryable, "flags_off layer should have queryable=false");
    assert!(l.opaque, "flags_off layer should have opaque=true");
}

#[test]
fn max_resolution_capped_fixture_sets_numeric_cap() {
    use kwms::config::model::MaxResolution;
    let (config, _) = common::load_fixture("max_resolution_capped");
    match &config.layers[0].max_resolution {
        MaxResolution::MetersPerPixel(v) => assert_eq!(*v, 0.5),
        other => panic!("expected MetersPerPixel(0.5); got {other:?}"),
    }
}

// -----------------------------------------------------------------------
// Env-var overrides over a real fixture file
// -----------------------------------------------------------------------

#[test]
fn env_var_overrides_apply_over_fixture() {
    let _g = ENV_LOCK.lock().unwrap();
    // Sanity: baseline reads default bind.
    let (baseline, _) = common::load_fixture("single_grid_minimal");
    assert_eq!(baseline.server.bind, "0.0.0.0:8080");

    // Override via env var, reload, and check.
    std::env::set_var("KWMS_SERVER__BIND", "127.0.0.1:7654");
    let (overridden, _) = common::load_fixture("single_grid_minimal");
    std::env::remove_var("KWMS_SERVER__BIND");

    assert_eq!(overridden.server.bind, "127.0.0.1:7654");
}

#[test]
fn env_var_override_for_nested_list_uses_yaml_scalar() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::set_var("KWMS_SERVER__MAX_REQUEST_PIXELS", "[4096, 4096]");
    let (config, _) = common::load_fixture("single_grid_minimal");
    std::env::remove_var("KWMS_SERVER__MAX_REQUEST_PIXELS");

    assert_eq!(config.server.max_request_pixels, [4096, 4096]);
}

// -----------------------------------------------------------------------
// Validation failures via load()
// -----------------------------------------------------------------------

fn write_tmp_yaml(s: &str) -> tempfile::NamedTempFile {
    let mut f = tempfile::NamedTempFile::new().expect("tempfile");
    f.write_all(s.as_bytes()).expect("write tmp yaml");
    f.flush().expect("flush");
    f
}

#[test]
fn load_surfaces_unknown_grid_error() {
    let _g = ENV_LOCK.lock().unwrap();
    let cfg = write_tmp_yaml(
        r#"
layers:
  - name: bad
    title: Bad
    grids: [definitely_not_a_grid]
    sources:
      - uri: x.tif
    formats: [png]
"#,
    );
    let no_secrets = std::env::temp_dir().join("kwms-no-such-secrets.yaml");
    let _ = std::fs::remove_file(&no_secrets);
    let err = kwms::config::load::load(cfg.path(), &no_secrets).unwrap_err();
    let s = format!("{err:#}");
    assert!(
        s.contains("definitely_not_a_grid"),
        "expected unknown-grid error to mention the bad name; got: {s}"
    );
}

#[test]
fn load_rejects_unknown_top_level_field() {
    // `deny_unknown_fields` on `Config` means a typo'd top-level key
    // surfaces as a parse error, not as silently-ignored config.
    let _g = ENV_LOCK.lock().unwrap();
    let cfg = write_tmp_yaml(
        r#"
serverr:  # typo
  bind: "0.0.0.0:8080"
"#,
    );
    let no_secrets = std::env::temp_dir().join("kwms-no-such-secrets.yaml");
    let _ = std::fs::remove_file(&no_secrets);
    let err = kwms::config::load::load(cfg.path(), &no_secrets).unwrap_err();
    let s = format!("{err:#}");
    assert!(
        s.contains("serverr") || s.contains("unknown field"),
        "expected unknown-field error to flag `serverr`; got: {s}"
    );
}

// -----------------------------------------------------------------------
// JSON Schema export
// -----------------------------------------------------------------------

#[test]
fn json_schema_export_is_valid_json() {
    let schema_text = emit_json_schema().expect("emit schema");
    let parsed: serde_json::Value =
        serde_json::from_str(&schema_text).expect("schema is valid JSON");

    // Top-level shape from schemars: $schema, title, type, properties, definitions.
    let obj = parsed.as_object().expect("top-level object");
    assert!(obj.contains_key("$schema"), "missing $schema");
    assert!(obj.contains_key("title"), "missing title");
    // schemars 0.8 emits `properties` at the root for the requested type.
    assert!(obj.contains_key("properties"), "missing properties");
}

#[test]
fn json_schema_mentions_known_layer_fields() {
    // We don't pin the exact schema shape — schemars output is not part of
    // any public contract — but it should at least describe a `layers`
    // array with the layer fields we know exist.
    let schema_text = emit_json_schema().expect("emit schema");
    for key in &["layers", "grids", "cache", "server", "formats", "queryable"] {
        assert!(
            schema_text.contains(key),
            "schema export missing reference to `{key}`; got:\n{schema_text}"
        );
    }
}
