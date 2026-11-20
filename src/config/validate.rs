//! Validation pass — structural + CRS-aware semantic checks.

use std::collections::HashSet;
use std::ops::RangeInclusive;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use gdal::errors::GdalError;
use gdal::raster::ResampleAlg;

use super::model::{Config, LayerConfig, LevelRule, OutputFormatsConfig, ServerConfig};
use crate::tile::grid::well_known_grid;

/// Run all validation rules. Returns the first error encountered, with context
/// naming the offending layer/field where possible.
pub fn validate(config: &Config) -> Result<()> {
    validate_server(&config.server).context("server")?;
    validate_output_formats(&config.output_formats).context("output_formats")?;
    validate_wmts(config).context("wmts")?;
    for (i, tier) in config.cache.tiers.iter().enumerate() {
        if let super::model::TierConfig::Memory(m) = tier {
            m.max_bytes
                .parse::<bytesize::ByteSize>()
                .map_err(|e| anyhow!("cache.tiers[{i}]: invalid max_bytes '{}': {e}", m.max_bytes))?;
        }
    }

    // Validate any custom grids' CRS via the crs module. This is the only
    // place we call into `crs::validate` and is exercised by integration tests.
    for (name, grid) in &config.grids {
        check_cache_segment("grid", name)?;
        if well_known_grid(name).is_some() {
            bail!("grid '{name}' would shadow the built-in grid of the same name; rename it");
        }
        crate::crs::validate(&grid.crs).with_context(|| format!("grid '{}': invalid crs '{}'", name, grid.crs))?;
        if grid.levels.is_some_and(|n| n == 0 || n > 30) {
            bail!("grid '{name}': levels must be 1-30");
        }
        if let Some([minx, miny, maxx, maxy]) = grid.extent {
            let [tlx, tly] = grid.top_left;
            let close = |a: f64, b: f64| (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0);
            if ![minx, miny, maxx, maxy].iter().all(|v| v.is_finite()) || minx >= maxx || miny >= maxy {
                bail!("grid '{name}': extent must be [minx, miny, maxx, maxy] with min < max");
            }
            if !close(minx, tlx) || !close(maxy, tly) || grid.origin != super::model::GridOrigin::TopLeft {
                bail!("grid '{name}': an extent's top-left [minx, maxy] must equal top_left (origin top_left)");
            }
        }
    }

    // Unique layer names.
    let mut seen = HashSet::new();
    for layer in &config.layers {
        check_cache_segment("layer", &layer.name)?;
        if !seen.insert(layer.name.as_str()) {
            bail!("duplicate layer name '{}'", layer.name);
        }
    }

    // The server opens one dataset per URI, so every source sharing a URI
    // must describe it identically.
    let mut by_uri = std::collections::HashMap::new();
    for layer in &config.layers {
        for source in &layer.sources {
            if let Some(first) = by_uri.insert(source.uri.as_str(), source) {
                if first != source {
                    bail!(
                        "layer '{}': source '{}' is declared elsewhere with different open_options or credentials",
                        layer.name,
                        source.uri
                    );
                }
            }
        }
    }

    // Per-layer checks.
    for layer in &config.layers {
        validate_layer(layer, config).with_context(|| format!("layer '{}'", layer.name))?;
    }

    Ok(())
}

/// Layer and grid names are directory names in the persistent cache
/// (`<layer>/<grid>/…`, see `cache::TileKey::as_path`), so each must be one
/// plain path segment: no `/`, no `..`, nothing an object store or file
/// system treats specially. A leading `.` is reserved for kwms's own objects
/// (the startup check).
fn check_cache_segment(kind: &str, name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if !ok {
        bail!(
            "{kind} name '{name}' must be 1-128 characters of A-Z, a-z, 0-9, '_', '-' or '.', \
             not starting with '.' (it is a directory name in the tile cache)"
        );
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
    if let Some(b) = layer.bbox {
        if ![b.minx, b.miny, b.maxx, b.maxy].iter().all(|v| v.is_finite()) || b.minx >= b.maxx || b.miny >= b.maxy {
            bail!("bbox must be [minx, miny, maxx, maxy] with min < max");
        }
    }
    if let Some(crs) = &layer.bbox_crs {
        if layer.bbox.is_none() {
            bail!("bbox_crs is set but bbox is not");
        }
        crate::crs::validate(crs).with_context(|| format!("bbox_crs '{crs}'"))?;
    }
    validate_output_formats(&OutputFormatsConfig::default().with_overrides(&layer.output_formats))
        .context("output_formats")?;
    if let Some(cc) = &layer.cache_control {
        validate_cache_control("cache_control", cc)?;
    }
    for source in &layer.sources {
        for option in &source.open_options {
            match option.split_once('=') {
                Some((key, _)) if !key.is_empty() && !option.contains('\0') => {}
                _ => bail!("source '{}': open option '{option}' is not KEY=VALUE", source.uri),
            }
        }
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
            bail!("grid '{}' is not a well-known name and is not defined in `grids:`", g);
        }
    }

    // Each grid gets its own cache directory (its EPSG code, else its
    // name — `cache::grid_dir`), so two grids of one layer in the same CRS
    // would share tiles.
    let mut dirs: std::collections::HashMap<String, &str> = std::collections::HashMap::new();
    for g in &layer.grids {
        let Some(grid) = crate::tile::grid::lookup_grid(g, &config.grids) else {
            continue; // reported above
        };
        let dir = crate::cache::grid_dir(g, grid.crs());
        if let Some(other) = dirs.insert(dir.clone(), g) {
            bail!(
                "grids '{other}' and '{g}' would share cache directory '{dir}' (same CRS); \
                 a layer can use one grid per CRS"
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

fn validate_wmts(config: &Config) -> Result<()> {
    let w = &config.wmts;
    // A plain absolute path: it becomes an axum route and part of every
    // advertised URL.
    let segment_ok = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    };
    let path_ok = w.path.starts_with('/') && w.path.len() > 1 && w.path[1..].split('/').all(segment_ok);
    if !path_ok {
        bail!(
            "path '{}' must be like /wmts or /hwh/luchtfotorgb/wmts/v1_0 (A-Z a-z 0-9 _ - . segments)",
            w.path
        );
    }
    if ["/wms", "/tms", "/healthz", "/readyz"]
        .iter()
        .any(|p| w.path == *p || w.path.starts_with(&format!("{p}/")))
    {
        bail!("path '{}' collides with another kwms endpoint", w.path);
    }
    if w.tile_matrix_digits > 9 {
        bail!("tile_matrix_digits must be 0-9");
    }
    let mut ids = HashSet::new();
    for t in &w.tile_matrix_sets {
        if well_known_grid(&t.grid).is_none() && !config.grids.contains_key(&t.grid) {
            bail!(
                "tile_matrix_sets: grid '{}' is not a well-known name and is not defined in `grids:`",
                t.grid
            );
        }
        // Becomes a URL path segment: no '/', '?', '#', whitespace or '%'.
        let id_ok = !t.identifier.is_empty()
            && t.identifier
                .chars()
                .all(|c| c.is_ascii_graphic() && !matches!(c, '/' | '?' | '#' | '%' | '&' | '<' | '>' | '"'));
        if !id_ok {
            bail!(
                "tile_matrix_sets: identifier '{}' must be printable ASCII without / ? # % & < > \"",
                t.identifier
            );
        }
        if !ids.insert(t.identifier.as_str()) {
            bail!("tile_matrix_sets: identifier '{}' is used twice", t.identifier);
        }
    }
    Ok(())
}

fn validate_output_formats(f: &OutputFormatsConfig) -> Result<()> {
    if !(1..=100).contains(&f.jpeg.quality) {
        bail!("jpeg.quality must be 1-100, got {}", f.jpeg.quality);
    }
    if !(1..=100).contains(&f.webp.quality) {
        bail!("webp.quality must be 1-100, got {}", f.webp.quality);
    }
    if !(1..=9).contains(&f.png.compression) {
        bail!("png.compression must be 1-9, got {}", f.png.compression);
    }
    if !(1..=3).contains(&f.geotiff.predictor) {
        bail!("geotiff.predictor must be 1-3, got {}", f.geotiff.predictor);
    }
    Ok(())
}

fn validate_server(server: &ServerConfig) -> Result<()> {
    if let Some(url) = &server.public_url {
        // Goes verbatim into every capabilities document.
        let rest = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"));
        let ok = rest.is_some_and(|r| {
            let host = r.split('/').next().unwrap_or_default();
            !host.is_empty()
                && r.chars()
                    .all(|c| c.is_ascii_graphic() && !matches!(c, '?' | '#' | '"' | '\'' | '<' | '>' | '&' | '\\'))
        });
        if !ok {
            bail!("public_url '{url}' must be an absolute http(s) URL without query or fragment, e.g. http://localhost:8080");
        }
    }
    crate::logging::parse_filter(&server.log_level)?;
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
    if server.source_timeout < std::time::Duration::from_secs(1) || server.source_timeout >= server.request_timeout {
        bail!(
            "source_timeout ({:?}) must be at least 1s and below request_timeout ({:?})",
            server.source_timeout,
            server.request_timeout
        );
    }
    if !(1..=256).contains(&server.tile_index_threads) {
        bail!("tile_index_threads must be between 1 and 256");
    }
    if server.header_read_timeout < std::time::Duration::from_secs(1) {
        bail!("header_read_timeout must be at least 1s");
    }
    if server.max_connections == 0 {
        bail!("max_connections must be positive");
    }
    validate_cache_control("server.cache_control.images", &server.cache_control.images)?;
    validate_cache_control("server.cache_control.capabilities", &server.cache_control.capabilities)?;
    Ok(())
}

/// A `Cache-Control` value goes verbatim into a response header: visible
/// ASCII and spaces only (no CR/LF), or empty for no header.
pub(crate) fn validate_cache_control(field: &str, value: &str) -> Result<()> {
    if value.len() > 256 || !value.chars().all(|c| c == ' ' || c.is_ascii_graphic()) {
        bail!("{field} must be a Cache-Control value of visible ASCII (at most 256 characters), e.g. 'public, max-age=3600'");
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
    use crate::config::model::{
        CacheMode, Config, LayerCacheConfig, LayerConfig, LevelRule, MaxResolution, SourceConfig,
    };
    use crate::encode::OutputFormat;
    use crate::tile::grid::WELL_KNOWN_GRIDS;

    /// Build a minimal, valid `LayerConfig` that uses only well-known grids
    /// (so validation never touches `crs::validate`).
    fn good_layer(name: &str) -> LayerConfig {
        LayerConfig {
            name: name.into(),
            grids: vec!["webmercator".into()],
            max_resolution: MaxResolution::default(),
            bbox: None,
            bbox_crs: None,
            sources: vec![SourceConfig {
                uri: "vsiaz://photos/x.vrt".into(),
                credentials: None,
                ..Default::default()
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
            source_mask: None,
            output_formats: Default::default(),
            cache_control: None,
        }
    }

    #[test]
    fn output_formats_layer_overrides_and_ranges() {
        let mut config: Config = serde_yaml::from_str(
            "output_formats: {jpeg: {quality: 70}, png: {compression: 9}}\n\
             layers:\n  - name: a\n    grids: [webmercator]\n    sources: [{uri: x}]\n    title: t\n    formats: [jpeg]\n\
             \x20   output_formats: {jpeg: {quality: 95, progressive: true}}\n",
        )
        .unwrap();
        let resolved = config.output_formats.with_overrides(&config.layers[0].output_formats);
        assert_eq!((resolved.jpeg.quality, resolved.jpeg.progressive), (95, true));
        assert_eq!(resolved.png.compression, 9, "not overridden: global setting");
        validate(&config).unwrap();
        config.output_formats.jpeg.quality = 0;
        assert!(format!("{:#}", validate(&config).unwrap_err()).contains("jpeg.quality"));
        config.output_formats.jpeg.quality = 80;
        config.layers[0].output_formats.png = Some(crate::config::model::PngSettings { compression: 12 });
        assert!(format!("{:#}", validate(&config).unwrap_err()).contains("png.compression"));
    }

    #[test]
    fn layer_source_mask_overrides_the_global_one() {
        let mut layer = good_layer("a");
        assert_eq!(layer.effective_source_mask(Some("global.shp")), Some("global.shp"));
        layer.source_mask = Some("own.gpkg".into());
        assert_eq!(layer.effective_source_mask(Some("global.shp")), Some("own.gpkg"));
        layer.source_mask = Some("none".into());
        assert_eq!(layer.effective_source_mask(Some("global.shp")), None);
        assert_eq!(good_layer("b").effective_source_mask(None), None);
    }

    #[test]
    fn public_url_must_be_absolute_http() {
        for good in ["http://localhost:8080", "https://tiles.example.org/kwms/"] {
            let mut c = cfg(vec![good_layer("a")]);
            c.server.public_url = Some(good.into());
            validate(&c).unwrap_or_else(|e| panic!("{good}: {e:#}"));
        }
        for bad in [
            "localhost:8080",
            "ftp://x",
            "http://",
            "http://x/?a=1",
            "http://x/#f",
            "http://a b",
            "http://x/\"<",
        ] {
            let mut c = cfg(vec![good_layer("a")]);
            c.server.public_url = Some(bad.into());
            assert!(validate(&c).is_err(), "{bad}");
        }
    }

    #[test]
    fn layer_names_must_be_plain_path_segments() {
        for bad in ["", "a/b", "..", ".hidden", "a b", "a\\b", "ünï"] {
            let err = validate(&cfg(vec![good_layer(bad)])).unwrap_err();
            assert!(
                format!("{err:#}").contains("directory name in the tile cache"),
                "{bad:?}: {err:#}"
            );
        }
        validate(&cfg(vec![good_layer("2026_orthoHR"), good_layer("ortho-2024.v2")])).unwrap();
    }

    #[test]
    fn two_grids_in_one_crs_are_rejected() {
        let mut config = cfg(vec![]);
        config.grids.insert(
            "my_rd".into(),
            crate::config::model::GridConfig {
                crs: crate::crs::Crs::new("EPSG:28992"),
                top_left: [-285401.92, 903401.92],
                tile_size: [256, 256],
                base_resolution: 3440.64,
                origin: Default::default(),
                extent: None,
                levels: None,
            },
        );
        let mut layer = good_layer("l");
        layer.grids = vec!["rd_new_quad".into(), "my_rd".into()];
        config.layers = vec![layer.clone()];
        let err = format!("{:#}", validate(&config).unwrap_err());
        assert!(err.contains("share cache directory '28992'"), "{err}");

        layer.grids = vec!["my_rd".into(), "webmercator".into()];
        config.layers = vec![layer];
        validate(&config).unwrap();
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
            LevelRule {
                levels: "0-5".into(),
                mode: CacheMode::Cache,
            },
            LevelRule {
                levels: "10".into(),
                mode: CacheMode::Passthrough,
            },
            LevelRule {
                levels: "18-21".into(),
                mode: CacheMode::Passthrough,
            },
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
            s.contains("layer 'ortho2024'") && s.contains("'foo'") && s.contains("does not match"),
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
                extent: None,
                levels: None,
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
            LevelRule {
                levels: "0-10".into(),
                mode: CacheMode::Cache,
            },
            LevelRule {
                levels: "5-15".into(),
                mode: CacheMode::Passthrough,
            },
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
