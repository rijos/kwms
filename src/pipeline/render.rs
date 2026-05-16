//! `render_tile` — produce a single encoded tile from a source through the cache.
//!
//! Flow (see `docs/src/caching.md` and `docs/src/scope-and-protocols.md`):
//!
//! 1. Build the [`TileKey`] from `(layer, coord, format, grid.crs())`.
//! 2. Resolve the effective cache mode for this tile's level via
//!    [`cache::policy::resolve_mode`].
//! 3. `Passthrough` levels render fresh on every request (no cache I/O).
//!    `Cache` levels go through `LayeredCache::get_or_compute`, which handles
//!    hot+persistent lookup and single-flight coalescing of concurrent misses.
//!
//! The compute closure is the same for both modes: look up the first source,
//! read a pixel window covering the tile's bbox at the grid's tile size,
//! attach georeferencing for GeoTIFF outputs only, then call the shared
//! [`crate::encode::encode`] entry point.

use std::sync::Arc;

use anyhow::{anyhow, Result};
use bytes::Bytes;

use crate::cache::{policy::resolve_mode, LayeredCache, TileKey};
use crate::config::{CacheMode, LayerConfig};
use crate::encode::{encode, EncodeOpts, GeoRef, OutputFormat};
use crate::source::{Resampling, SourceRegistry, WindowRequest};
use crate::tile::{TileCoord, TileGrid};

/// Render (or fetch from cache) a single tile, returning encoded bytes.
pub async fn render_tile(
    sources: &SourceRegistry,
    cache: &LayeredCache,
    layer: &LayerConfig,
    grid: &Arc<dyn TileGrid>,
    coord: TileCoord,
    format: OutputFormat,
) -> Result<Bytes> {
    let key = TileKey {
        layer: layer.name.clone(),
        coord,
        format,
        crs: grid.crs().clone(),
    };

    let mode = resolve_mode(&layer.cache, coord.z);

    match mode {
        CacheMode::Passthrough => compute_tile(sources, layer, grid, coord, format).await,
        CacheMode::Cache => {
            cache
                .get_or_compute(key, layer.cache.ttl, || {
                    compute_tile(sources, layer, grid, coord, format)
                })
                .await
        }
    }
}

/// Render the tile fresh from the source and encode it. Shared between the
/// passthrough and cache-miss paths.
async fn compute_tile(
    sources: &SourceRegistry,
    layer: &LayerConfig,
    grid: &Arc<dyn TileGrid>,
    coord: TileCoord,
    format: OutputFormat,
) -> Result<Bytes> {
    // Multi-source compositing is out of scope for v1 — take the first source
    // and let the validator catch empty `sources:` blocks at load time.
    let source_cfg = layer
        .sources
        .first()
        .ok_or_else(|| anyhow!("layer `{}` has no sources configured", layer.name))?;
    let source = sources.get(&source_cfg.uri).ok_or_else(|| {
        anyhow!(
            "source `{}` referenced by layer `{}` not present in registry",
            source_cfg.uri,
            layer.name
        )
    })?;

    let (tile_w, tile_h) = grid.tile_size();
    let bbox = grid.tile_bbox(coord);
    let req = WindowRequest {
        bbox,
        crs: grid.crs().clone(),
        width: tile_w,
        height: tile_h,
        resampling: resampling_from_str(&layer.resampling),
    };
    let buffer = source.read_window(req).await?;

    // GeoTIFF gets georeferencing; raster web formats do not.
    let georef = match format {
        OutputFormat::GeoTiff => Some(GeoRef {
            crs: grid.crs().clone(),
            bbox,
            nodata: buffer.nodata,
        }),
        _ => None,
    };

    encode(&buffer, georef.as_ref(), format, &EncodeOpts::defaults_for(format))
}

/// Map the per-layer `resampling:` string to the public `Resampling` enum.
///
/// Unknown values default to [`Resampling::Bilinear`] — the config validator
/// rejects unknown spellings on load, so this branch should never fire in
/// practice and serves only as a defensive fallback.
pub fn resampling_from_str(s: &str) -> Resampling {
    match s.trim().to_ascii_lowercase().as_str() {
        "nearest" => Resampling::Nearest,
        "bilinear" => Resampling::Bilinear,
        "cubic" => Resampling::Cubic,
        "cubic_spline" | "cubicspline" => Resampling::CubicSpline,
        "lanczos" => Resampling::Lanczos,
        "average" => Resampling::Average,
        "mode" => Resampling::Mode,
        _ => Resampling::Bilinear,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;
    use std::time::Duration;

    use opendal::{services, Operator};
    use tempfile::NamedTempFile;

    use crate::cache::hot::HotTier;
    use crate::cache::persistent::PersistentTier;
    use crate::config::model::{
        LayerCacheConfig, MaxResolution, MaxResolutionMode, SourceConfig,
    };
    use crate::crs::{Bbox, Crs};
    use crate::source::gdal::GdalSource;
    use crate::source::{PixelBuffer, PixelType, Source};
    use crate::tile::grid::WebMercator;

    /// Write a small EPSG:4326 RGB GeoTIFF to a temp file. Bbox is the whole
    /// world `[-180, -90, 180, 90]` so the WebMercator level-0 tile (which
    /// covers the full mercator extent) intersects the fixture and warping
    /// has something to read.
    fn write_world_rgb_fixture() -> NamedTempFile {
        let width: u32 = 64;
        let height: u32 = 32;
        let bands: u32 = 3;
        let mut data: Vec<u8> = Vec::with_capacity((width * height * bands) as usize);
        for row in 0..height {
            for col in 0..width {
                data.push((col as u8).wrapping_mul(4));
                data.push((row as u8).wrapping_mul(8));
                data.push(((row + col) as u8).wrapping_mul(2));
            }
        }
        let buffer = PixelBuffer {
            width,
            height,
            bands,
            pixel_type: PixelType::U8,
            data: Bytes::from(data),
            nodata: None,
        };
        let georef = GeoRef {
            crs: Crs::new("EPSG:4326"),
            bbox: Bbox::from([-180.0, -90.0, 180.0, 90.0]),
            nodata: None,
        };
        let bytes = encode(
            &buffer,
            Some(&georef),
            OutputFormat::GeoTiff,
            &EncodeOpts::defaults_for(OutputFormat::GeoTiff),
        )
        .expect("encode world fixture");

        let mut tmp = tempfile::Builder::new()
            .prefix("kwms-pipeline-render-")
            .suffix(".tif")
            .tempfile()
            .expect("tempfile");
        tmp.write_all(&bytes).expect("write fixture");
        tmp.flush().expect("flush fixture");
        tmp
    }

    fn fresh_cache() -> LayeredCache {
        let op = Operator::new(services::Memory::default())
            .expect("memory builder")
            .finish();
        LayeredCache::from_parts(HotTier::new(8 * 1024 * 1024), PersistentTier::new(op))
    }

    fn build_layer(name: &str, source_uri: &str) -> LayerConfig {
        LayerConfig {
            name: name.into(),
            grids: vec!["webmercator".into()],
            max_resolution: MaxResolution::Mode(MaxResolutionMode::Native),
            bbox: None,
            sources: vec![SourceConfig {
                uri: source_uri.into(),
                credentials: None,
            }],
            cache: LayerCacheConfig {
                mode: CacheMode::Cache,
                ttl: Duration::from_secs(60),
                level_rules: Vec::new(),
            },
            title: name.into(),
            abstract_: String::new(),
            keywords: Vec::new(),
            attribution: None,
            metadata_url: None,
            formats: vec![OutputFormat::Png],
            resampling: "bilinear".into(),
            queryable: true,
            opaque: false,
            nodata: None,
        }
    }

    #[tokio::test]
    async fn render_tile_returns_png_and_caches_second_call() {
        let tmp = write_world_rgb_fixture();
        let uri = tmp.path().to_string_lossy().into_owned();

        let mut registry = SourceRegistry::new();
        let source: Arc<dyn Source> = Arc::new(
            GdalSource::open(&SourceConfig {
                uri: uri.clone(),
                credentials: None,
            })
            .expect("open fixture as GdalSource"),
        );
        registry.insert(uri.clone(), source);

        let cache = fresh_cache();
        let layer = build_layer("world", &uri);
        let grid: Arc<dyn TileGrid> = Arc::new(WebMercator::new());
        let coord = TileCoord::new(0, 0, 0);

        let first = render_tile(&registry, &cache, &layer, &grid, coord, OutputFormat::Png)
            .await
            .expect("first render");
        assert!(first.len() > 8, "PNG output too short: {} bytes", first.len());
        assert_eq!(
            &first[..4],
            &[0x89, 0x50, 0x4E, 0x47],
            "PNG magic missing"
        );

        // Second call should hit the cache and return identical bytes.
        let second = render_tile(&registry, &cache, &layer, &grid, coord, OutputFormat::Png)
            .await
            .expect("second render");
        assert_eq!(first, second, "cache should return identical bytes");
    }

    #[tokio::test]
    async fn render_tile_passthrough_does_not_populate_cache() {
        let tmp = write_world_rgb_fixture();
        let uri = tmp.path().to_string_lossy().into_owned();

        let mut registry = SourceRegistry::new();
        let source: Arc<dyn Source> = Arc::new(
            GdalSource::open(&SourceConfig {
                uri: uri.clone(),
                credentials: None,
            })
            .expect("open fixture"),
        );
        registry.insert(uri.clone(), source);

        let cache = fresh_cache();
        let mut layer = build_layer("world", &uri);
        layer.cache.mode = CacheMode::Passthrough;

        let grid: Arc<dyn TileGrid> = Arc::new(WebMercator::new());
        let coord = TileCoord::new(0, 0, 0);

        let bytes = render_tile(&registry, &cache, &layer, &grid, coord, OutputFormat::Png)
            .await
            .expect("passthrough render");
        assert!(!bytes.is_empty());

        // Cache should still be empty for the matching key.
        let key = TileKey {
            layer: layer.name.clone(),
            coord,
            format: OutputFormat::Png,
            crs: grid.crs().clone(),
        };
        let got = cache
            .get(&key, Duration::from_secs(60))
            .await
            .expect("cache get ok");
        assert!(got.is_none(), "passthrough must not populate the cache");
    }

    #[test]
    fn resampling_from_str_known_and_unknown() {
        assert!(matches!(resampling_from_str("nearest"), Resampling::Nearest));
        assert!(matches!(resampling_from_str("BILINEAR"), Resampling::Bilinear));
        assert!(matches!(resampling_from_str("cubic"), Resampling::Cubic));
        assert!(matches!(
            resampling_from_str("cubic_spline"),
            Resampling::CubicSpline
        ));
        assert!(matches!(resampling_from_str("lanczos"), Resampling::Lanczos));
        assert!(matches!(resampling_from_str("average"), Resampling::Average));
        assert!(matches!(resampling_from_str("mode"), Resampling::Mode));
        // Unknown → bilinear fallback.
        assert!(matches!(
            resampling_from_str("not-a-thing"),
            Resampling::Bilinear
        ));
    }
}
