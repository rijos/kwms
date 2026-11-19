//! `render_tile` — produce a single encoded tile from a source through the cache.
//!
//! Flow (see `docs/src/caching.md` and `docs/src/scope-and-protocols.md`):
//!
//! 1. Reject requests the layer doesn't offer: a tile outside the grid's
//!    pyramid, or a format missing from the layer's `formats:` list.
//! 2. Tiles that don't touch the source's coverage are served as a fill tile
//!    (per `docs/src/per-layer-behavior.md`) *without* reading the source or
//!    writing the cache — otherwise any client could fill the persistent
//!    cache with empty tiles from the whole world.
//! 3. Build the [`TileKey`] from `(layer, grid dir, coord, format)` and
//!    resolve the effective cache mode for this tile's level via
//!    [`cache::policy::resolve_mode`].
//! 4. `Passthrough` levels render fresh on every request (no cache I/O).
//!    `Cache` levels go through every cache tier, `Memory` levels (and
//!    `Cache` levels deeper than the layer's `cache.max_level`) through the
//!    in-process tiers only — both via `LayeredCache::get_or_compute_in`,
//!    which handles lookup, write-through and single-flight coalescing.
//!
//! The compute closure is the same for both modes: look up the first source,
//! read a pixel window covering the tile's bbox at the grid's tile size,
//! attach georeferencing for GeoTIFF outputs only, then encode on the
//! bounded [`CpuPool`].

use std::collections::HashMap;
use std::sync::Arc;

use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use gdal::raster::ResampleAlg;

use super::cpu::CpuPool;
use super::read_budget;
use crate::cache::{grid_dir, policy::resolve_mode, Computed, LayeredCache, Scope, TileKey};
use crate::config::{CacheMode, LayerConfig};
use crate::crs::{transform_bbox, Bbox, Crs};
use crate::encode::{encode, EncodeOpts, GeoRef, OutputFormat};
use crate::protocol::query::TileQuery;
use crate::source::{Area, PixelBuffer, Source, SourceRegistry, WindowRequest};
use crate::tile::{TileCoord, TileGrid};

/// Everything tile rendering needs, shared by all request handlers.
pub struct Renderer {
    pub sources: Arc<SourceRegistry>,
    pub cache: Arc<LayeredCache>,
    pub cpu: CpuPool,
    /// Source extent per `(source uri, grid CRS)`, computed once at
    /// construction (GDAL/PROJ never run for this on request threads) and
    /// read-only afterwards, so lookups need no lock.
    coverage: HashMap<(String, Crs), Bbox>,
    /// Deepest durably cached level per `(layer, grid)` and why
    /// (`cache.max_level`, see [`super::cache_levels`]). A pair not listed
    /// has no limit.
    cache_limits: HashMap<(String, String), (Option<u32>, String)>,
    /// Each layer's `source_mask` (its own or the global one), loaded at
    /// startup. A layer not listed has none.
    masks: HashMap<String, Area>,
    /// The areas that decide where a layer has data, per CRS: its source's
    /// coverage and its mask, reprojected once (on the blocking pool).
    coverage_parts: std::sync::Mutex<HashMap<(String, Crs), CoverageParts>>,
    /// Encoder settings per layer (`output_formats`, global with the
    /// layer's overrides). A layer not listed uses kwms's defaults.
    encoding: HashMap<String, crate::config::OutputFormatsConfig>,
}

/// Coverage areas of one layer in one CRS.
type CoverageParts = Arc<Vec<Area>>;

impl Renderer {
    /// Build a renderer for every `(source uri, grid)` pair in `serves`:
    ///
    /// * computes the source's coverage in the grid's CRS — a transform GDAL
    ///   can't do is an error here, never a silent "assume covered" at
    ///   request time;
    /// * checks that every tile of the grid can be rendered within the
    ///   source-read budget ([`read_budget`]) — a source without the
    ///   overviews for that stops startup ("overviews or bail").
    pub fn new(
        sources: Arc<SourceRegistry>,
        cache: Arc<LayeredCache>,
        cpu: CpuPool,
        serves: impl IntoIterator<Item = (String, Arc<dyn TileGrid>)>,
    ) -> Result<Self> {
        let mut coverage = HashMap::new();
        for (uri, grid) in serves {
            let Some(source) = sources.get(&uri) else { continue };
            let meta = source.metadata();
            let crs = grid.crs().clone();
            let extent = match coverage.get(&(uri.clone(), crs.clone())) {
                Some(extent) => *extent,
                None if meta.native_crs == crs => meta.native_bbox,
                None => transform_bbox(&meta.native_bbox, &meta.native_crs, &crs)
                    .with_context(|| format!("coverage of source `{uri}` in {crs}"))?,
            };
            read_budget::check(&uri, meta, grid.as_ref(), &extent)?;
            coverage.insert((uri, crs), extent);
        }
        Ok(Self {
            sources,
            cache,
            cpu,
            coverage,
            cache_limits: HashMap::new(),
            masks: HashMap::new(),
            coverage_parts: std::sync::Mutex::new(HashMap::new()),
            encoding: HashMap::new(),
        })
    }

    /// The precomputed extent of source `uri` in `crs`.
    fn coverage(&self, uri: &str, crs: &Crs) -> Result<&Bbox> {
        self.coverage
            .get(&(uri.to_string(), crs.clone()))
            .ok_or_else(|| anyhow!("internal: no coverage computed for source `{uri}` in {crs}"))
    }
}

impl Renderer {
    /// Set each layer's encoder settings.
    pub fn with_encoding(mut self, encoding: HashMap<String, crate::config::OutputFormatsConfig>) -> Self {
        self.encoding = encoding;
        self
    }

    /// The encoder options `layer` uses for `format`.
    pub(crate) fn encode_opts(&self, layer: &LayerConfig, format: OutputFormat) -> EncodeOpts {
        match self.encoding.get(&layer.name) {
            Some(cfg) => EncodeOpts::from_config(format, cfg),
            None => EncodeOpts::defaults_for(format),
        }
    }

    /// Set each layer's `source_mask` area.
    pub fn with_masks(mut self, masks: HashMap<String, Area>) -> Self {
        self.masks = masks;
        self
    }

    /// The areas a pixel of `layer` in `crs` must lie in to have data: the
    /// source's coverage (tile footprints / extent) and the layer's mask.
    /// Reprojected once per (layer, CRS), off the async workers.
    pub(crate) async fn coverage_parts(&self, layer: &LayerConfig, crs: &Crs) -> Result<Arc<Vec<Area>>> {
        let key = (layer.name.clone(), crs.clone());
        if let Some(parts) = self
            .coverage_parts
            .lock()
            .map_err(|_| anyhow!("coverage cache poisoned"))?
            .get(&key)
        {
            return Ok(parts.clone());
        }
        let source_area = layer
            .sources
            .first()
            .and_then(|s| self.sources.get(&s.uri))
            .and_then(|s| s.coverage_area());
        let areas: Vec<Area> = source_area
            .into_iter()
            .chain(self.masks.get(&layer.name).cloned())
            .collect();
        let target = crs.clone();
        let parts = tokio::task::spawn_blocking(move || -> Result<Vec<Area>> {
            areas
                .iter()
                .map(|a| crate::source::gdal::mask::reproject(a, &target))
                .collect()
        })
        .await
        .context("reprojecting coverage areas")??;
        let parts = Arc::new(parts);
        self.coverage_parts
            .lock()
            .map_err(|_| anyhow!("coverage cache poisoned"))?
            .insert(key, parts.clone());
        Ok(parts)
    }

    /// Set the durable-cache depth per `(layer, grid)` (see
    /// [`super::cache_levels::layer_max_level`]).
    pub fn with_cache_limits(mut self, limits: HashMap<(String, String), (Option<u32>, String)>) -> Self {
        self.cache_limits = limits;
        self
    }

    /// The deepest durably cached level of `layer` in `grid` and why;
    /// `None` when unlimited.
    pub fn cache_limit(&self, layer: &str, grid: &str) -> Option<&(Option<u32>, String)> {
        self.cache_limits.get(&(layer.to_string(), grid.to_string()))
    }

    /// Whether a request for `bbox` at `size` in `grid` is finer than the
    /// deepest durably cached level of `layer` — beyond the cached pyramid.
    /// Always `false` for a pair without a limit.
    pub fn finer_than_cached(&self, layer: &str, grid: &dyn TileGrid, bbox: &Bbox, size: (u32, u32)) -> bool {
        let Some((max, _)) = self.cache_limit(layer, grid.name()) else {
            return false;
        };
        match max {
            None => true,
            // A hair of tolerance: a request exactly at that level's
            // resolution is served from its tiles.
            Some(max) => crate::tile::pyramid::requested_resolution(bbox, size) < grid.resolution(*max) * (1.0 - 1e-9),
        }
    }

    /// Whether a direct read of `bbox` (in `crs`) at `size` from `layer`'s
    /// source stays within [`read_budget::MAX_DIRECT_READ_PIXELS`]. Request
    /// sizes are client-chosen, so a source with sparse overviews must not
    /// turn one GetMap into a huge read; such requests use tiles instead.
    pub fn direct_read_fits(&self, layer: &LayerConfig, crs: &Crs, bbox: &Bbox, size: (u32, u32)) -> Result<bool> {
        let Some(source) = layer.sources.first().and_then(|s| self.sources.get(&s.uri)) else {
            return Ok(false);
        };
        Ok(read_budget::window_read_pixels(source.metadata(), crs, bbox, size)? <= read_budget::MAX_DIRECT_READ_PIXELS)
    }

    /// Whether tiles of `level` go to the durable tiers.
    fn stores_durably(&self, layer: &str, grid: &str, level: u32) -> bool {
        match self.cache_limit(layer, grid) {
            None => true,
            Some((max, _)) => max.is_some_and(|max| level <= max),
        }
    }

    /// Nodata for a layer: its `nodata:` override, else its first source's.
    pub(crate) fn layer_nodata(&self, layer: &LayerConfig) -> Option<f64> {
        layer.nodata.or_else(|| {
            let uri = &layer.sources.first()?.uri;
            self.sources.get(uri)?.metadata().nodata
        })
    }
}

fn intersects(a: &Bbox, b: &Bbox) -> bool {
    a.minx < b.maxx && b.minx < a.maxx && a.miny < b.maxy && b.miny < a.maxy
}

/// Render (or fetch from cache) a single tile, returning encoded bytes.
///
/// Takes only a validated [`TileQuery`]: layer, grid and coordinate are
/// catalog entries already checked by the protocol layer.
pub async fn render_tile(r: &Renderer, query: &TileQuery<'_>) -> Result<Bytes> {
    render_grid_tile(
        r,
        query.layer().config(),
        query.grid().grid(),
        query.coord(),
        query.format(),
    )
    .await
}

/// Render one tile of `grid` for an internal caller (WMS compositing),
/// whose coordinates the server computed itself from a validated request.
/// The range/format checks here are invariants, not request validation:
/// a failure is a server bug and is reported as an internal error.
pub(crate) async fn render_grid_tile(
    r: &Renderer,
    layer: &LayerConfig,
    grid: &Arc<dyn TileGrid>,
    coord: TileCoord,
    format: OutputFormat,
) -> Result<Bytes> {
    grid.check_coord(coord)
        .map_err(|e| anyhow!("internal: tile outside grid `{}`: {e}", grid.name()))?;
    if !layer.formats.contains(&format) {
        return Err(anyhow!(
            "internal: format {} not offered by layer `{}`",
            format.mime_type(),
            layer.name
        ));
    }

    // Multi-source compositing is out of scope for v1 — take the first source
    // and let the validator catch empty `sources:` blocks at load time.
    let uri = &layer
        .sources
        .first()
        .ok_or_else(|| anyhow!("layer `{}` has no sources configured", layer.name))?
        .uri;
    let source = r.sources.get(uri).ok_or_else(|| {
        anyhow!(
            "source `{uri}` referenced by layer `{}` not present in registry",
            layer.name
        )
    })?;

    let bbox = grid.tile_bbox(coord);
    {
        let cov = r.coverage(uri, grid.crs())?;
        if !intersects(&bbox, cov) {
            let (w, h) = grid.tile_size();
            return fill_image(r, source.as_ref(), layer, grid.crs(), bbox, (w, h), format).await;
        }
    }

    let key = TileKey {
        layer: layer.name.clone(),
        coord,
        format,
        grid_dir: grid_dir(grid.name(), grid.crs()),
    };

    // Below `cache.max_level` tiles render about as fast from the source
    // as they'd load from storage: keep them in memory only.
    let mode = match resolve_mode(&layer.cache, coord.z) {
        CacheMode::Cache if !r.stores_durably(&layer.name, grid.name(), coord.z) => CacheMode::Memory,
        mode => mode,
    };
    let scope = match mode {
        CacheMode::Passthrough => return Ok(compute_tile(r, source, layer, grid, bbox, format).await?.bytes),
        CacheMode::Cache => Scope::All,
        CacheMode::Memory => Scope::InProcess,
    };
    r.cache
        .get_or_compute_in(scope, key, layer.cache.ttl, || {
            compute_tile(r, source.clone(), layer, grid, bbox, format)
        })
        .await
}

/// Read a whole image straight from the source: `bbox` in `crs` (the
/// CRS of `grid`) at exactly `size`, in one GDAL warp, encoded on the
/// render pool. Not cached. WMS uses it for requests finer than the
/// layer's durably cached levels ([`Renderer::finer_than_cached`]), where
/// composing from tiles would mean many uncached tile reads plus a second
/// resampling. A bbox that misses the source gets a fill image without
/// touching GDAL.
pub(crate) async fn render_window(
    r: &Renderer,
    layer: &LayerConfig,
    grid: &Arc<dyn TileGrid>,
    crs: &Crs,
    bbox: Bbox,
    size: (u32, u32),
    format: OutputFormat,
) -> Result<Bytes> {
    let uri = &layer
        .sources
        .first()
        .ok_or_else(|| anyhow!("layer `{}` has no sources", layer.name))?
        .uri;
    let source = r
        .sources
        .get(uri)
        .ok_or_else(|| anyhow!("source `{uri}` of layer `{}` is not open", layer.name))?;
    if !intersects(&bbox, r.coverage(uri, grid.crs())?) {
        return fill_image(r, source.as_ref(), layer, crs, bbox, size, format).await;
    }
    let started = std::time::Instant::now();
    let req = WindowRequest {
        bbox,
        crs: crs.clone(),
        width: size.0,
        height: size.1,
        resampling: layer_resampling(layer)?,
        nodata: layer.nodata,
    };
    let parts = r.coverage_parts(layer, crs).await?;
    let buffer = source.read_window(req).await?;
    let make = move || crate::source::gdal::mask::apply(buffer, &parts, &bbox);
    let opts = r.encode_opts(layer, format);
    let encoded = encode_on_pool(r, make, crs.clone(), bbox, format, opts).await?;
    super::stage::record("wms_direct", started);
    Ok(encoded.bytes)
}

/// What [`seed_grid_tile`] did with one tile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeedOutcome {
    /// Already in the persistent cache and within its TTL; left alone.
    Fresh,
    /// Rendered and written to the persistent cache.
    Stored,
    /// Rendered, but all nodata: not cached (see `compute_tile`).
    Empty,
    /// Outside the source's coverage: nothing to render or cache.
    Outside,
}

/// Fill the persistent cache with one tile for `kwms cache update`: skip it
/// if a fresh copy is cached (unless `force`), otherwise render it from the
/// source and write it. Same key, same rendering and same "no empty tiles"
/// rule as serving, so a seeded tile is exactly what a request would have
/// cached, written to every durable tier. Cache errors are returned, not
/// degraded.
pub async fn seed_grid_tile(
    r: &Renderer,
    layer: &LayerConfig,
    grid: &Arc<dyn TileGrid>,
    coord: TileCoord,
    format: OutputFormat,
    force: bool,
) -> Result<SeedOutcome> {
    grid.check_coord(coord)
        .map_err(|e| anyhow!("tile outside grid `{}`: {e}", grid.name()))?;
    let uri = &layer
        .sources
        .first()
        .ok_or_else(|| anyhow!("layer `{}` has no sources", layer.name))?
        .uri;
    let source = r
        .sources
        .get(uri)
        .ok_or_else(|| anyhow!("source `{uri}` of layer `{}` is not open", layer.name))?;
    let bbox = grid.tile_bbox(coord);
    if !intersects(&bbox, r.coverage(uri, grid.crs())?) {
        return Ok(SeedOutcome::Outside);
    }
    let key = TileKey {
        layer: layer.name.clone(),
        grid_dir: grid_dir(grid.name(), grid.crs()),
        coord,
        format,
    };
    if !force && r.cache.durable_fresh(&key, layer.cache.ttl).await? {
        return Ok(SeedOutcome::Fresh);
    }
    let Computed { bytes, store } = compute_tile(r, source, layer, grid, bbox, format).await?;
    if !store {
        return Ok(SeedOutcome::Empty);
    }
    r.cache.store_durable(&key, bytes).await?;
    Ok(SeedOutcome::Stored)
}

/// The source extent of `uri` in `crs`, as computed at startup.
impl Renderer {
    pub fn source_coverage(&self, uri: &str, crs: &Crs) -> Result<Bbox> {
        self.coverage(uri, crs).copied()
    }
}

/// Render the tile fresh from the source and encode it. Shared between the
/// passthrough and cache-miss paths.
///
/// A tile that came back entirely nodata is marked transient — returned
/// (also to concurrent requests for it) but not cached: the startup
/// coverage is a superset of the data, so without this any client could
/// fill the persistent cache with empty tiles from inside that superset.
async fn compute_tile(
    r: &Renderer,
    source: Arc<dyn Source>,
    layer: &LayerConfig,
    grid: &Arc<dyn TileGrid>,
    bbox: Bbox,
    format: OutputFormat,
) -> Result<Computed> {
    let (tile_w, tile_h) = grid.tile_size();
    let req = WindowRequest {
        bbox,
        crs: grid.crs().clone(),
        width: tile_w,
        height: tile_h,
        resampling: layer_resampling(layer)?,
        nodata: layer.nodata,
    };
    let parts = r.coverage_parts(layer, grid.crs()).await?;
    let buffer = source.read_window(req).await?;
    let make = move || crate::source::gdal::mask::apply(buffer, &parts, &bbox);
    let opts = r.encode_opts(layer, format);
    let encoded = encode_on_pool(r, make, grid.crs().clone(), bbox, format, opts).await?;
    Ok(if encoded.all_fill {
        Computed::transient(encoded.bytes)
    } else {
        Computed::store(encoded.bytes)
    })
}

/// A `size` image with no data in the layer's band layout (transparent in
/// PNG, white in JPEG) — the response for a WMS request whose BBOX misses
/// the grid entirely.
pub(crate) async fn blank_image(
    r: &Renderer,
    layer: &LayerConfig,
    crs: &Crs,
    bbox: Bbox,
    size: (u32, u32),
    format: OutputFormat,
) -> Result<Bytes> {
    let uri = &layer
        .sources
        .first()
        .ok_or_else(|| anyhow!("layer `{}` has no sources configured", layer.name))?
        .uri;
    let source = r
        .sources
        .get(uri)
        .ok_or_else(|| anyhow!("source `{uri}` of layer `{}` not in registry", layer.name))?;
    fill_image(r, source.as_ref(), layer, crs, bbox, size, format).await
}

/// An image entirely outside the source: no data in the source's band
/// layout (transparent in PNG, white in JPEG). Not cached — it costs no
/// source I/O to produce again.
///
/// The buffer (up to `max_request_pixels` × bands × sample size) is created
/// inside the render-pool job, so it only exists while the request holds a
/// render slot: requests queued for a slot hold no image memory.
async fn fill_image(
    r: &Renderer,
    source: &dyn Source,
    layer: &LayerConfig,
    crs: &Crs,
    bbox: Bbox,
    (w, h): (u32, u32),
    format: OutputFormat,
) -> Result<Bytes> {
    let meta = source.metadata();
    let (bands, pixel_type) = (meta.band_count, meta.pixel_type);
    let make = move || crate::source::gdal::mask::empty_image(w, h, bands, pixel_type);
    let opts = r.encode_opts(layer, format);
    Ok(encode_on_pool(r, make, crs.clone(), bbox, format, opts).await?.bytes)
}

/// An encoded image, and whether every pixel was the fill value.
struct Encoded {
    bytes: Bytes,
    all_fill: bool,
}

/// Produce a buffer with `make` and encode it, both on the bounded render
/// pool.
async fn encode_on_pool<F>(
    r: &Renderer,
    make: F,
    crs: Crs,
    bbox: Bbox,
    format: OutputFormat,
    opts: EncodeOpts,
) -> Result<Encoded>
where
    F: FnOnce() -> Result<PixelBuffer> + Send + 'static,
{
    r.cpu
        .run("encode", move || {
            let buffer = make()?;
            // GeoTIFF gets georeferencing; raster web formats do not.
            let georef = match format {
                OutputFormat::GeoTiff => Some(GeoRef {
                    crs,
                    bbox,
                    nodata: buffer.nodata,
                }),
                _ => None,
            };
            let bytes = encode(&buffer, georef.as_ref(), format, &opts)?;
            Ok(Encoded {
                bytes,
                all_fill: buffer.is_all_fill(),
            })
        })
        .await
}

/// The layer's resampling kernel as GDAL's enum. Validation already ran
/// `ResampleAlg::from_str` on it, so failure here means a config bypassed it.
pub(crate) fn layer_resampling(layer: &LayerConfig) -> Result<ResampleAlg> {
    ResampleAlg::from_str(&layer.resampling).map_err(|e| anyhow!("layer `{}`: invalid resampling: {e}", layer.name))
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;
    use std::time::Duration;

    use tempfile::NamedTempFile;

    use crate::cache::memory::MemoryTier;
    use crate::config::model::{LayerCacheConfig, MaxResolution, MaxResolutionMode, SourceConfig};
    use crate::crs::{Bbox, Crs};
    use crate::pipeline::cpu::CpuPool;
    use crate::source::gdal::GdalSource;
    use crate::source::{PixelBuffer, PixelType, Source};
    use crate::tile::grid::WebMercator;

    /// Write a small EPSG:4326 RGB GeoTIFF to a temp file. Bbox is the whole
    /// world `[-180, -90, 180, 90]` so the WebMercator level-0 tile (which
    /// covers the full mercator extent) intersects the fixture and warping
    /// has something to read.
    fn write_world_rgb_fixture() -> NamedTempFile {
        write_rgb_fixture(Bbox::from([-180.0, -90.0, 180.0, 90.0]))
    }

    /// Same as [`write_world_rgb_fixture`] but covering only `bbox` (EPSG:4326).
    fn write_rgb_fixture(bbox: Bbox) -> NamedTempFile {
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
            alpha: None,
        };
        let georef = GeoRef {
            crs: Crs::new("EPSG:4326"),
            bbox,
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
        LayeredCache::from_tiers(vec![std::sync::Arc::new(MemoryTier::new(8 * 1024 * 1024))])
    }

    fn test_renderer(registry: SourceRegistry, uri: &str) -> Renderer {
        Renderer::new(
            Arc::new(registry),
            Arc::new(fresh_cache()),
            CpuPool::new(2, Duration::from_secs(5)),
            [(uri.to_string(), Arc::new(WebMercator::new()) as Arc<dyn TileGrid>)],
        )
        .expect("renderer")
    }

    fn open_registry(uri: &str) -> SourceRegistry {
        let mut registry = SourceRegistry::new();
        let source: Arc<dyn Source> = Arc::new(
            GdalSource::open(&SourceConfig {
                uri: uri.into(),
                credentials: None,
                ..Default::default()
            })
            .expect("open fixture as GdalSource"),
        );
        registry.insert(uri.into(), source);
        registry
    }

    fn build_layer(name: &str, source_uri: &str) -> LayerConfig {
        LayerConfig {
            name: name.into(),
            grids: vec!["webmercator".into()],
            max_resolution: MaxResolution::Mode(MaxResolutionMode::Native),
            bbox: None,
            bbox_crs: None,
            sources: vec![SourceConfig {
                uri: source_uri.into(),
                credentials: None,
                ..Default::default()
            }],
            cache: LayerCacheConfig {
                mode: CacheMode::Cache,
                max_level: crate::config::model::CacheMaxLevel::default(),
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
            source_mask: None,
            output_formats: Default::default(),
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
                ..Default::default()
            })
            .expect("open fixture as GdalSource"),
        );
        registry.insert(uri.clone(), source);

        let r = test_renderer(registry, &uri);
        let layer = build_layer("world", &uri);
        let grid: Arc<dyn TileGrid> = Arc::new(WebMercator::new());
        let coord = TileCoord::new(0, 0, 0);

        let first = render_grid_tile(&r, &layer, &grid, coord, OutputFormat::Png)
            .await
            .expect("first render");
        assert!(first.len() > 8, "PNG output too short: {} bytes", first.len());
        assert_eq!(&first[..4], &[0x89, 0x50, 0x4E, 0x47], "PNG magic missing");

        // Second call should hit the cache and return identical bytes.
        let second = render_grid_tile(&r, &layer, &grid, coord, OutputFormat::Png)
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
                ..Default::default()
            })
            .expect("open fixture"),
        );
        registry.insert(uri.clone(), source);

        let r = test_renderer(registry, &uri);
        let mut layer = build_layer("world", &uri);
        layer.cache.mode = CacheMode::Passthrough;

        let grid: Arc<dyn TileGrid> = Arc::new(WebMercator::new());
        let coord = TileCoord::new(0, 0, 0);

        let bytes = render_grid_tile(&r, &layer, &grid, coord, OutputFormat::Png)
            .await
            .expect("passthrough render");
        assert!(!bytes.is_empty());

        // Cache should still be empty for the matching key.
        let key = TileKey {
            layer: layer.name.clone(),
            coord,
            format: OutputFormat::Png,
            grid_dir: grid_dir(grid.name(), grid.crs()),
        };
        let got = r.cache.get(&key, Duration::from_secs(60)).await.expect("cache get ok");
        assert!(got.is_none(), "passthrough must not populate the cache");
    }

    #[tokio::test]
    async fn internal_entry_point_still_refuses_invalid_tiles() {
        // Request validation lives in the protocol layer (TileQuery); this
        // checks the defensive invariant for internal callers. A violation
        // is a server bug → Internal (never shown to clients), not Client.
        use crate::pipeline::error::{classify, Disposition};

        let tmp = write_world_rgb_fixture();
        let uri = tmp.path().to_string_lossy().into_owned();
        let r = test_renderer(open_registry(&uri), &uri);
        let layer = build_layer("world", &uri);
        let grid: Arc<dyn TileGrid> = Arc::new(WebMercator::new());

        for coord in [
            TileCoord::new(22, 0, 0),
            TileCoord::new(64, 0, 0),
            TileCoord::new(1, 2, 0),
        ] {
            let err = render_grid_tile(&r, &layer, &grid, coord, OutputFormat::Png)
                .await
                .unwrap_err();
            assert_eq!(classify(&err), Disposition::Internal, "{coord:?}: {err:#}");
        }

        // Layer offers PNG only; GeoTIFF (raw data) must not be served.
        let err = render_grid_tile(&r, &layer, &grid, TileCoord::new(0, 0, 0), OutputFormat::GeoTiff)
            .await
            .unwrap_err();
        assert_eq!(classify(&err), Disposition::Internal, "{err:#}");
    }

    #[tokio::test]
    async fn tiles_outside_source_coverage_are_filled_and_not_cached() {
        // Source covers roughly the Netherlands only.
        let tmp = write_rgb_fixture(Bbox::from([3.0, 50.0, 7.5, 54.0]));
        let uri = tmp.path().to_string_lossy().into_owned();
        let r = test_renderer(open_registry(&uri), &uri);
        let layer = build_layer("nl", &uri);
        let grid: Arc<dyn TileGrid> = Arc::new(WebMercator::new());

        // Level 10, far south-west of the coverage (Atlantic / Antarctica).
        let outside = TileCoord::new(10, 10, 1000);
        let bytes = render_grid_tile(&r, &layer, &grid, outside, OutputFormat::Png)
            .await
            .expect("out-of-coverage tile renders as fill");
        assert_eq!(&bytes[..4], &[0x89, 0x50, 0x4E, 0x47], "PNG magic");

        let key = TileKey {
            layer: layer.name.clone(),
            coord: outside,
            format: OutputFormat::Png,
            grid_dir: grid_dir(grid.name(), grid.crs()),
        };
        let cached = r.cache.get(&key, Duration::from_secs(60)).await.unwrap();
        assert!(cached.is_none(), "out-of-coverage tiles must not be cached");
    }

    #[test]
    fn layer_resampling_uses_gdal_names() {
        let mut layer = build_layer("x", "memory://x");
        layer.resampling = "nearestneighbour".into();
        assert_eq!(layer_resampling(&layer).unwrap(), ResampleAlg::NearestNeighbour);
        layer.resampling = "nonsense".into();
        assert!(layer_resampling(&layer).is_err());
    }

    #[tokio::test]
    async fn all_nodata_tiles_inside_the_coverage_are_served_but_not_cached() {
        // World extent (so both z1 tiles are inside the coverage), but data
        // only in the eastern half; the western half is nodata.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("east-only.tif");
        {
            let drv = gdal::DriverManager::get_driver_by_name("GTiff").unwrap();
            let mut ds = drv.create_with_band_type::<u8, _>(&path, 64, 32, 1).unwrap();
            ds.set_spatial_ref(&crate::source::gdal::raster::gis_order_spatial_ref(&Crs::new("EPSG:4326")).unwrap())
                .unwrap();
            ds.set_geo_transform(&[-180.0, 360.0 / 64.0, 0.0, 90.0, 0.0, -180.0 / 32.0])
                .unwrap();
            let mut band = ds.rasterband(1).unwrap();
            band.set_no_data_value(Some(0.0)).unwrap();
            let pixels: Vec<u8> = (0..32)
                .flat_map(|_| (0..64).map(|x| if x < 32 { 0 } else { 200 }))
                .collect();
            band.write((0, 0), (64, 32), &mut gdal::raster::Buffer::new((64, 32), pixels))
                .unwrap();
        }
        let uri = path.to_string_lossy().into_owned();
        let r = test_renderer(open_registry(&uri), &uri);
        let layer = build_layer("east", &uri);
        let grid: Arc<dyn TileGrid> = Arc::new(WebMercator::new());
        let key = |x| TileKey {
            layer: "east".into(),
            coord: TileCoord::new(1, x, 0),
            format: OutputFormat::Png,
            grid_dir: grid_dir(grid.name(), grid.crs()),
        };
        for x in [0, 1] {
            render_grid_tile(&r, &layer, &grid, TileCoord::new(1, x, 0), OutputFormat::Png)
                .await
                .unwrap();
        }
        assert!(
            r.cache.get(&key(0), layer.cache.ttl).await.unwrap().is_none(),
            "empty tile was cached"
        );
        assert!(
            r.cache.get(&key(1), layer.cache.ttl).await.unwrap().is_some(),
            "data tile was not cached"
        );
    }
}
