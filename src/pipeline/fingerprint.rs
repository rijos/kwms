//! A fingerprint of everything that decides a layer's pixels, stored with
//! each durably cached tile (`.meta` sidecar) so `kwms cache update` can
//! tell a tile made with other settings from a current one.
//!
//! Covered: the layer's sources (URIs, open options), resampling, nodata,
//! grids (and custom grid definitions), effective encoder settings, the
//! `source_mask` file's *contents*, and [`RENDER_REVISION`]. Not covered:
//! titles, abstracts, metadata URLs, cache settings, `Cache-Control` —
//! nothing that changes a tile.
//!
//! Serving deliberately ignores the fingerprint (a known trade-off): after
//! a settings change, cached tiles stay in use until the next cache update
//! (or until their TTL), so a deploy never triggers a re-render storm.

use std::collections::HashMap;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::config::{Config, GridConfig, LayerConfig, OutputFormatsConfig};

/// Bumped when kwms renders the same settings differently (e.g. the GTI
/// overview geometry fix), so the next cache update replaces every tile.
pub const RENDER_REVISION: u32 = 2;

#[derive(Serialize)]
struct RenderInputs<'a> {
    revision: u32,
    sources: &'a [crate::config::SourceConfig],
    resampling: &'a str,
    nodata: Option<f64>,
    grids: Vec<(&'a str, Option<&'a GridConfig>)>,
    encoding: OutputFormatsConfig,
    max_resolution: &'a crate::config::MaxResolution,
    mask: Option<String>,
}

/// The fingerprint of every layer, by name (hex, 16 digits).
pub fn layer_fingerprints(config: &Config) -> Result<HashMap<String, String>> {
    let mut masks: HashMap<String, String> = HashMap::new();
    let mut out = HashMap::new();
    for layer in &config.layers {
        let mask = match layer.effective_source_mask(config.source_mask.as_deref()) {
            Some(path) => {
                if !masks.contains_key(path) {
                    let digest = file_set_digest(path).with_context(|| format!("hashing source_mask '{path}'"))?;
                    masks.insert(path.to_string(), digest);
                }
                masks.get(path).cloned()
            }
            None => None,
        };
        out.insert(layer.name.clone(), fingerprint(config, layer, mask)?);
    }
    Ok(out)
}

fn fingerprint(config: &Config, layer: &LayerConfig, mask: Option<String>) -> Result<String> {
    let inputs = RenderInputs {
        revision: RENDER_REVISION,
        sources: &layer.sources,
        resampling: &layer.resampling,
        nodata: layer.nodata,
        grids: layer.grids.iter().map(|g| (g.as_str(), config.grids.get(g))).collect(),
        encoding: config.output_formats.with_overrides(&layer.output_formats),
        max_resolution: &layer.max_resolution,
        mask,
    };
    let canonical = serde_json::to_vec(&inputs).context("serializing render settings")?;
    Ok(format!("{:016x}", fnv1a(&canonical)))
}

/// Digest of a vector file and its usual sidecars (`.shp` + `.shx`, `.dbf`,
/// `.prj`, …): editing a mask in place changes it.
fn file_set_digest(path: &str) -> Result<String> {
    let main = std::path::Path::new(path);
    let mut bytes = std::fs::read(main).with_context(|| format!("reading '{path}'"))?;
    for ext in ["shx", "dbf", "prj", "cpg"] {
        if let Ok(more) = std::fs::read(main.with_extension(ext)) {
            bytes.extend_from_slice(ext.as_bytes());
            bytes.extend_from_slice(&more);
        }
    }
    Ok(format!("{:016x}", fnv1a(&bytes)))
}

/// FNV-1a, 64 bit: stable across Rust versions and platforms (unlike
/// `DefaultHasher`), which a fingerprint persisted in the cache needs.
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= u64::from(*b);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(yaml: &str) -> Config {
        serde_yaml::from_str(yaml).unwrap()
    }

    const BASE: &str = r#"
layers:
  - name: a
    title: A
    grids: [webmercator]
    formats: [png]
    sources: [{uri: "x.tif"}]
"#;

    #[test]
    fn render_settings_change_the_fingerprint_metadata_does_not() {
        let base = layer_fingerprints(&config(BASE)).unwrap()["a"].clone();
        let retitled = BASE.replace("title: A", "title: B\n    abstract: about");
        assert_eq!(layer_fingerprints(&config(&retitled)).unwrap()["a"], base);
        let resource = BASE.replace("x.tif", "y.tif");
        assert_ne!(layer_fingerprints(&config(&resource)).unwrap()["a"], base);
        let quality = format!("{BASE}output_formats:\n  png:\n    compression: 9\n");
        assert_ne!(layer_fingerprints(&config(&quality)).unwrap()["a"], base);
    }

    #[test]
    fn mask_contents_count() {
        let dir = tempfile::tempdir().unwrap();
        let shp = dir.path().join("m.shp");
        std::fs::write(&shp, b"one").unwrap();
        let yaml = format!("source_mask: {}\n{BASE}", shp.display());
        let first = layer_fingerprints(&config(&yaml)).unwrap()["a"].clone();
        std::fs::write(&shp, b"two").unwrap();
        assert_ne!(layer_fingerprints(&config(&yaml)).unwrap()["a"], first);
    }

    #[test]
    fn fnv_is_stable() {
        assert_eq!(fnv1a(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
