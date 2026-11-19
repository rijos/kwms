//! Shared harness for integration tests under `tests/`.
//!
//! Each `tests/*.rs` file is its own test binary; they pull in this module
//! with `mod common;`. Files under `tests/common/` aren't compiled as
//! binaries by themselves, so this file just provides helpers.
//!
//! Capabilities tests bootstrap their pdok XML on first run by setting
//! `UPDATE_PDOK=1`. CI runs without the env var, so any future drift
//! against the checked-in pdok files fails the build.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, Response};
use axum::Router;
use bytes::Bytes;
use tower::ServiceExt;

use kwms::cache::layered::LayeredCache;
use kwms::cache::memory::MemoryTier;
use kwms::config::load::load;
use kwms::config::{Config, Secrets};
use kwms::server::{router, AppState};
use kwms::source::SourceRegistry;

// ---------------------------------------------------------------------------
// Filesystem layout
// ---------------------------------------------------------------------------

pub fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

pub fn fixture_config_path(name: &str) -> PathBuf {
    fixtures_dir().join("config").join(format!("{name}.yaml"))
}

pub fn fixture_secrets_path(name: &str) -> PathBuf {
    fixtures_dir().join("config").join(format!("{name}.secrets.yaml"))
}

pub fn pdok_path(rel: impl AsRef<Path>) -> PathBuf {
    fixtures_dir().join("pdok").join(rel)
}

pub fn sources_dir() -> PathBuf {
    fixtures_dir().join("sources")
}

// ---------------------------------------------------------------------------
// Config / AppState
// ---------------------------------------------------------------------------

/// Load a fixture `config.yaml` + (optional) `secrets.yaml` from
/// `tests/fixtures/config/`. Panics on failure — the fixture set is meant
/// to be valid.
pub fn load_fixture(name: &str) -> (Config, Secrets) {
    let cfg = fixture_config_path(name);
    let secrets = fixture_secrets_path(name);
    load(&cfg, &secrets).unwrap_or_else(|e| {
        panic!("loading fixture `{name}` failed: {e:#}");
    })
}

/// Build an `AppState` from a fixture config **without** opening any GDAL
/// sources. Use this for tests that exercise capabilities rendering,
/// config logic, or cache behavior — anything that doesn't need a real
/// raster on disk.
pub fn state_without_sources(name: &str) -> Arc<AppState> {
    let (config, _secrets) = load_fixture(name);
    Arc::new(
        AppState::from_parts(config, SourceRegistry::new(), memory_cache(), 0)
            .unwrap_or_else(|e| panic!("building state for fixture `{name}`: {e:#}")),
    )
}

/// Build an `AppState` with **real** GDAL-backed sources. The fixture's
/// referenced URIs must exist on disk (the fixture generator at
/// `tools/gen_fixtures.rs` produces them). Use this for `tile_render.rs`
/// and `conformance.rs` GetMap/GetTile tests.
pub fn state_with_sources(name: &str) -> Arc<AppState> {
    let (config, secrets) = load_fixture(name);
    let cache = memory_cache();
    let mut registry = SourceRegistry::new();
    let mut seen = std::collections::HashSet::new();
    for layer in &config.layers {
        for source_cfg in &layer.sources {
            if !seen.insert(source_cfg.uri.clone()) {
                continue;
            }
            let src = kwms::source::gdal::GdalSource::open(source_cfg).unwrap_or_else(|e| {
                panic!(
                    "opening source `{}` for fixture `{name}` failed: {e:#}\n\
                     (have you run `cargo run --bin gen_fixtures` to generate \
                     the .tif rasters under tests/fixtures/sources/?)",
                    source_cfg.uri
                )
            });
            registry.insert(source_cfg.uri.clone(), Arc::new(src) as Arc<dyn kwms::source::Source>);
        }
    }
    let source_count = seen.len();
    let _ = secrets;
    Arc::new(
        AppState::from_parts(config, registry, cache, source_count)
            .unwrap_or_else(|e| panic!("building state for fixture `{name}`: {e:#}")),
    )
}

/// An in-memory `LayeredCache` — backed by opendal's `Memory` service for
/// the persistent tier. Useful when tests want the cache plumbing to work
/// without touching the filesystem.
pub fn memory_cache() -> LayeredCache {
    LayeredCache::from_tiers(vec![std::sync::Arc::new(MemoryTier::new(64 * 1024 * 1024))])
}

/// Build the production axum router around the given state.
pub fn app(state: Arc<AppState>) -> Router {
    router(state)
}

// ---------------------------------------------------------------------------
// HTTP request helpers
// ---------------------------------------------------------------------------

pub async fn get(app: Router, uri: &str) -> Response<Body> {
    app.oneshot(Request::builder().uri(uri).body(Body::empty()).expect("build request"))
        .await
        .expect("oneshot")
}

pub async fn body_string(resp: Response<Body>) -> String {
    let bytes = body_bytes(resp).await;
    String::from_utf8_lossy(&bytes).to_string()
}

pub async fn body_bytes(resp: Response<Body>) -> Bytes {
    axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024)
        .await
        .expect("read response body")
}

pub fn content_type(resp: &Response<Body>) -> String {
    resp.headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string()
}

// ---------------------------------------------------------------------------
// Pdok-file workflow
// ---------------------------------------------------------------------------

/// Compare `produced` XML against the pdok file at
/// `tests/fixtures/pdok/<rel>` (after whitespace normalization).
///
/// Bootstrap / update workflow: set `UPDATE_PDOK=1` and re-run the
/// test. The produced (normalized) output is written over the pdok file
/// and the assertion is skipped. Commit the resulting diff after review.
pub fn assert_xml_matches_pdok(produced: &str, rel: &str) {
    let pdok = pdok_path(rel);
    let produced_norm = normalize_xml(produced);

    if std::env::var("UPDATE_PDOK").is_ok() {
        if let Some(parent) = pdok.parent() {
            std::fs::create_dir_all(parent).expect("create pdok parent dir");
        }
        std::fs::write(&pdok, &produced_norm).expect("write pdok file");
        eprintln!("(UPDATE_PDOK) wrote {}", pdok.display());
        return;
    }

    let expected = std::fs::read_to_string(&pdok).unwrap_or_else(|e| {
        panic!(
            "missing pdok file `{}` ({e}). \
             Bootstrap with UPDATE_PDOK=1 and commit the result.",
            pdok.display()
        )
    });
    let expected_norm = normalize_xml(&expected);

    if expected_norm != produced_norm {
        pretty_assertions::assert_eq!(
            expected_norm,
            produced_norm,
            "XML differs from pdok `{}`. \
             If intended, regenerate with UPDATE_PDOK=1.",
            pdok.display()
        );
    }
}

/// Compare `produced` bytes against the pdok file at
/// `tests/fixtures/pdok/<rel>` (byte-exact).
///
/// Same bootstrap workflow as [`assert_xml_matches_pdok`].
pub fn assert_bytes_match_pdok(produced: &[u8], rel: &str) {
    let pdok = pdok_path(rel);

    if std::env::var("UPDATE_PDOK").is_ok() {
        if let Some(parent) = pdok.parent() {
            std::fs::create_dir_all(parent).expect("create pdok parent dir");
        }
        std::fs::write(&pdok, produced).expect("write pdok file");
        eprintln!("(UPDATE_PDOK) wrote {}", pdok.display());
        return;
    }

    let expected = std::fs::read(&pdok).unwrap_or_else(|e| {
        panic!(
            "missing pdok file `{}` ({e}). \
             Bootstrap with UPDATE_PDOK=1 and commit the result.",
            pdok.display()
        )
    });

    if expected != produced {
        panic!(
            "byte mismatch against pdok `{}` (expected {} bytes, got {} bytes). \
             If intended, regenerate with UPDATE_PDOK=1.",
            pdok.display(),
            expected.len(),
            produced.len()
        );
    }
}

/// Normalize XML so trivial formatting differences don't fail tests.
///
/// Rules:
///   1. Whitespace between tags (`>   <`) → none.
///   2. Whitespace inside tag bodies → collapsed to single spaces.
///   3. Whitespace inside text nodes → collapsed to single spaces.
///   4. Leading/trailing whitespace trimmed.
///
/// We deliberately do **not** sort attributes or normalize namespace
/// prefixes: kwms's generators emit them in a stable order, and changes
/// in either are real, semantically-meaningful drift worth flagging.
pub fn normalize_xml(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    let mut last_was_space = true; // suppress leading whitespace

    for c in s.chars() {
        if c == '<' {
            // Drop any trailing whitespace we accumulated before this tag.
            while out.ends_with(' ') {
                out.pop();
            }
            in_tag = true;
            last_was_space = false;
            out.push(c);
        } else if c == '>' {
            in_tag = false;
            // Pretend we just emitted whitespace so leading whitespace
            // in the following text node gets suppressed (matching the
            // "no whitespace between tags" rule).
            last_was_space = true;
            out.push(c);
        } else if c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
            // Inside or outside a tag, runs of whitespace collapse to one
            // space; the tag-open branch strips trailing space between
            // tags entirely.
            let _ = in_tag;
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
    out.trim().to_string()
}

#[cfg(test)]
mod self_tests {
    use super::normalize_xml;

    #[test]
    fn normalize_collapses_inter_tag_whitespace() {
        let a = normalize_xml("<a>\n  <b>x</b>\n</a>");
        let b = normalize_xml("<a><b>x</b></a>");
        assert_eq!(a, b);
    }

    #[test]
    fn normalize_collapses_attribute_whitespace() {
        let a = normalize_xml("<a   x=\"1\"   y=\"2\"/>");
        let b = normalize_xml("<a x=\"1\" y=\"2\"/>");
        assert_eq!(a, b);
    }

    #[test]
    fn normalize_preserves_attribute_order() {
        let a = normalize_xml("<a x=\"1\" y=\"2\"/>");
        let b = normalize_xml("<a y=\"2\" x=\"1\"/>");
        assert_ne!(a, b, "attribute order is meaningful drift");
    }

    #[test]
    fn normalize_collapses_text_whitespace() {
        let a = normalize_xml("<a>  hello\n  world  </a>");
        let b = normalize_xml("<a>hello world</a>");
        assert_eq!(a, b);
    }
}
