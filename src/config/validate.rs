//! Validation pass — structural + CRS-aware semantic checks.

use std::collections::HashSet;
use std::ops::RangeInclusive;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use gdal::errors::GdalError;
use gdal::raster::ResampleAlg;

use super::model::{Config, LayerConfig, LevelRule, ServerConfig};
use crate::tile::grid::well_known_grid;

/// Run all validation rules. Returns the first error encountered, with context
/// naming the offending layer/field where possible.
pub fn validate(config: &Config) -> Result<()> {
    validate_server(&config.server).context("server")?;

    // Validate any custom grids' CRS via the crs module. This is the only
    // place we call into `crs::validate` and is exercised by integration tests.
    for (name, grid) in &config.grids {
        if well_known_grid(name).is_some() {
            bail!("grid '{name}' would shadow the built-in grid of the same name; rename it");
        }
        crate::crs::validate(&grid.crs)
            .with_context(|| format!("grid '{}': invalid crs '{}'", name, grid.crs))?;
    }

    // Unique layer names.
    let mut seen = HashSet::new();
    for layer in &config.layers {
        if !seen.insert(layer.name.as_str()) {
            bail!("duplicate layer name '{}'", layer.name);
        }
    }

    // Per-layer checks.
    for layer in &config.layers {
        validate_layer(layer, config)
            .with_context(|| format!("layer '{}'", layer.name))?;
    }

    Ok(())
}

fn validate_layer(layer: &LayerConfig, config: &Config) -> Result<()> {
    if layer.formats.is_empty() {
        bail!("formats must be non-empty");
    }
    if layer.sources.is_empty() {
        bail!("sources must be non-empty");
    }

    // Grid references — must be either a well-known name or a key in
    // `config.grids`.
    if layer.grids.is_empty() {
        bail!("grids must be non-empty");
    }
    for g in &layer.grids {
        let is_well_known = well_known_grid(g).is_some();
        let is_custom = config.grids.contains_key(g);
        if !is_well_known && !is_custom {
            bail!(
                "grid '{}' is not a well-known name and is not defined in `grids:`",
                g
            );
        }
    }

    // Cache TTL must be positive.
    if layer.cache.ttl == Duration::ZERO {
        bail!("cache.ttl must be > 0");
    }

    // Level rule ranges must parse and not overlap within one layer.
    validate_level_rules(&layer.cache.level_rules)?;

    // Resampling kernel — delegated to GDAL. The token in `config.yaml` must
    // be one GDAL recognises (e.g. `nearestneighbour`, `bilinear`, `cubic`,
    // `cubicspline`, `lanczos`, `average`, `mode`, `gauss`); see GDAL's
    // `ResampleAlg::from_str` for the exact list. `GdalError`'s Display impl
    // omits the descriptive payload, so we unwrap the `BadArgument` variant
    // to keep the kernel list in the user-facing message.
    let alg = ResampleAlg::from_str(&layer.resampling).map_err(|e| match e {
        GdalError::BadArgument(msg) => anyhow!("resampling {}", msg),
        other => anyhow!("resampling '{}': {other:?}", layer.resampling),
    })?;
    // All reads go through the GDAL warper, which has no Gauss kernel.
    crate::source::gdal::raster::warp_resampling_name(alg)?;

    Ok(())
}

fn validate_server(server: &ServerConfig) -> Result<()> {
    let [w, h] = server.max_request_pixels;
    if w == 0 || h == 0 {
        bail!("max_request_pixels must be positive, got [{w}, {h}]");
    }
    if server.max_concurrent_requests == 0 {
        bail!("max_concurrent_requests must be positive");
    }
    if server.render_concurrency == Some(0) {
        bail!("render_concurrency must be positive");
    }
    if server.request_timeout.is_zero() || server.render_queue_timeout.is_zero() {
        bail!("request_timeout and render_queue_timeout must be positive");
    }
    if server.metrics_bind.as_deref() == Some(server.bind.as_str()) {
        bail!("metrics_bind must differ from bind ('{}')", server.bind);
    }
    Ok(())
}

fn validate_level_rules(rules: &[LevelRule]) -> Result<()> {
    let mut parsed: Vec<RangeInclusive<u32>> = Vec::with_capacity(rules.len());
    for rule in rules {
        let range = parse_level_range(&rule.levels)
            .with_context(|| format!("cache.level_rules: invalid range '{}'", rule.levels))?;
        // Check overlap against earlier ranges.
        for existing in &parsed {
            if ranges_overlap(existing, &range) {
                bail!(
                    "cache.level_rules: range '{}-{}' overlaps with '{}-{}'",
                    range.start(),
                    range.end(),
                    existing.start(),
                    existing.end()
                );
            }
        }
        parsed.push(range);
    }
    Ok(())
}

fn parse_level_range(s: &str) -> Result<RangeInclusive<u32>> {
    let s = s.trim();
    if let Some((lo, hi)) = s.split_once('-') {
        let lo: u32 = lo
            .trim()
            .parse()
            .map_err(|_| anyhow!("range start is not a non-negative integer: '{}'", lo))?;
        let hi: u32 = hi
            .trim()
            .parse()
            .map_err(|_| anyhow!("range end is not a non-negative integer: '{}'", hi))?;
        if lo > hi {
            bail!("range start ({}) > range end ({})", lo, hi);
        }
        Ok(lo..=hi)
    } else {
        let v: u32 = s
            .parse()
            .map_err(|_| anyhow!("level is not a non-negative integer: '{}'", s))?;
        Ok(v..=v)
    }
}

fn ranges_overlap(a: &RangeInclusive<u32>, b: &RangeInclusive<u32>) -> bool {
    a.start() <= b.end() && b.start() <= a.end()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile::grid::WELL_KNOWN_GRIDS;
    use crate::config::model::{
        CacheMode, Config, LayerCacheConfig, LayerConfig, LevelRule, MaxResolution, SourceConfig,
    };
    use crate::encode::OutputFormat;

    /// Build a minimal, valid `LayerConfig` that uses only well-known grids
    /// (so validation never touches `crs::validate`).
    fn good_layer(name: &str) -> LayerConfig {
        LayerConfig {
            name: name.into(),
            grids: vec!["webmercator".into()],
            max_resolution: MaxResolution::default(),
            bbox: None,
            sources: vec![SourceConfig {
                uri: "vsiaz://photos/x.vrt".into(),
                credentials: None,
            }],
            cache: LayerCacheConfig::default(),
            title: "T".into(),
            abstract_: String::new(),
            keywords: vec![],
            attribution: None,
            metadata_url: None,
            formats: vec![OutputFormat::Png],
            resampling: "bilinear".into(),
            queryable: true,
            opaque: false,
            nodata: None,
        }
    }

    fn cfg(layers: Vec<LayerConfig>) -> Config {
        Config {
            layers,
            ..Default::default()
        }
    }

    #[test]
    fn valid_config_passes() {
        let mut l = good_layer("a");
        l.cache.level_rules = vec![
            LevelRule { levels: "0-5".into(), mode: CacheMode::Cache },
            LevelRule { levels: "10".into(), mode: CacheMode::Passthrough },
            LevelRule { levels: "18-21".into(), mode: CacheMode::Passthrough },
        ];
        validate(&cfg(vec![l])).expect("should be valid");
    }

    #[test]
    fn duplicate_layer_names_fails() {
        let l1 = good_layer("a");
        let l2 = good_layer("a");
        let err = validate(&cfg(vec![l1, l2])).unwrap_err();
        assert!(err.to_string().contains("duplicate layer name 'a'"), "got: {err}");
    }

    #[test]
    fn empty_formats_fails() {
        let mut l = good_layer("a");
        l.formats.clear();
        let err = validate(&cfg(vec![l])).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("formats must be non-empty"), "got: {s}");
        assert!(s.contains("layer 'a'"), "got: {s}");
    }

    #[test]
    fn empty_sources_fails() {
        let mut l = good_layer("a");
        l.sources.clear();
        let err = validate(&cfg(vec![l])).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("sources must be non-empty"), "got: {s}");
    }

    #[test]
    fn unknown_grid_fails() {
        let mut l = good_layer("a");
        l.grids = vec!["nope_grid".into()];
        let err = validate(&cfg(vec![l])).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("grid 'nope_grid'"), "got: {s}");
    }

    #[test]
    fn all_well_known_grids_pass() {
        // Sanity: every name in our list should validate. Use one layer per
        // name to keep names unique.
        for (i, g) in WELL_KNOWN_GRIDS.iter().enumerate() {
            let mut l = good_layer(&format!("layer{i}"));
            l.grids = vec![g.name.into()];
            validate(&cfg(vec![l])).expect("well-known grid must pass");
        }
    }

    #[test]
    fn zero_ttl_fails() {
        let mut l = good_layer("a");
        l.cache.ttl = Duration::ZERO;
        let err = validate(&cfg(vec![l])).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("cache.ttl"), "got: {s}");
    }

    #[test]
    fn unsupported_resampling_fails() {
        let mut l = good_layer("ortho2024");
        l.resampling = "foo".into();
        let err = validate(&cfg(vec![l])).unwrap_err();
        let s = format!("{err:#}");
        // Message is sourced from GDAL — it says `'foo' does not match one of [...]`
        // and lists the supported kernels. We assert on the offending token
        // and the "does not match" phrase rather than the exact list, so a
        // future GDAL release adding a kernel doesn't break this test.
        assert!(
            s.contains("layer 'ortho2024'")
                && s.contains("'foo'")
                && s.contains("does not match"),
            "got: {s}"
        );
    }

    #[test]
    fn each_supported_resampling_passes() {
        // Source of truth: whatever GDAL's `ResampleAlg::from_str` accepts,
        // minus kernels the GDAL warper lacks (only `gauss`).
        for (i, alg) in ResampleAlg::iter().enumerate() {
            let token = alg.to_string().to_lowercase();
            let mut l = good_layer(&format!("l{i}"));
            l.resampling = token.clone();
            let result = validate(&cfg(vec![l]));
            if alg == ResampleAlg::Gauss {
                let err = format!("{:#}", result.unwrap_err());
                assert!(err.contains("not supported by the GDAL warper"), "{err}");
            } else {
                result.unwrap_or_else(|e| panic!("token `{token}` must validate: {e:#}"));
            }
        }
    }

    #[test]
    fn custom_grid_cannot_shadow_a_built_in() {
        let mut c = cfg(vec![good_layer("a")]);
        c.grids.insert(
            "webmercator".into(),
            crate::config::GridConfig {
                crs: crate::crs::Crs::new("EPSG:3857"),
                top_left: [0.0, 0.0],
                tile_size: [256, 256],
                base_resolution: 1.0,
                origin: Default::default(),
            },
        );
        let err = format!("{:#}", validate(&c).unwrap_err());
        assert!(err.contains("shadow the built-in"), "{err}");
    }

    #[test]
    fn malformed_level_range_fails() {
        let mut l = good_layer("a");
        l.cache.level_rules = vec![LevelRule {
            levels: "not-a-range".into(),
            mode: CacheMode::Cache,
        }];
        let err = validate(&cfg(vec![l])).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("level_rules"), "got: {s}");
    }

    #[test]
    fn overlapping_level_ranges_fail() {
        let mut l = good_layer("a");
        l.cache.level_rules = vec![
            LevelRule { levels: "0-10".into(), mode: CacheMode::Cache },
            LevelRule { levels: "5-15".into(), mode: CacheMode::Passthrough },
        ];
        let err = validate(&cfg(vec![l])).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("overlaps"), "got: {s}");
    }

    #[test]
    fn single_level_range_parses() {
        let mut l = good_layer("a");
        l.cache.level_rules = vec![LevelRule {
            levels: "15".into(),
            mode: CacheMode::Cache,
        }];
        validate(&cfg(vec![l])).expect("single level must parse");
    }

    #[test]
    fn empty_grids_fails() {
        let mut l = good_layer("a");
        l.grids.clear();
        let err = validate(&cfg(vec![l])).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("grids must be non-empty"), "got: {s}");
    }

    #[test]
    fn parse_level_range_inverted_fails() {
        let err = parse_level_range("20-10").unwrap_err();
        assert!(err.to_string().contains("range start"), "got: {err}");
    }
}
