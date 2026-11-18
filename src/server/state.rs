//! Application state shared by all axum handlers.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::cache::LayeredCache;
use crate::config::{Config, Secrets};
use crate::pipeline::{CpuPool, Renderer};
use crate::protocol::catalog::Catalog;
use crate::source::{gdal::GdalSource, Source, SourceRegistry};

/// Shared, cheaply-cloneable state injected into every axum handler.
///
/// Construction is a synchronous, eager affair: we open one dataset per
/// unique source URI at startup so bad URIs / missing credentials surface
/// before the listener is bound (per the readiness story in
/// `docs/src/observability.md`).
#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    /// What requests may refer to; protocol validators resolve against it.
    pub catalog: Arc<Catalog>,
    /// Sources, cache and the bounded render pool.
    pub renderer: Arc<Renderer>,
    /// Capabilities documents, rendered once at startup.
    pub documents: Arc<Documents>,
    /// Number of unique source URIs registered. Tracked here because
    /// [`SourceRegistry`] doesn't (yet) expose a count — and the readiness
    /// endpoint needs to answer "is at least one source warmed?" without
    /// poking through the trait.
    pub source_count: usize,
}

impl AppState {
    /// Build the full app state from a parsed config + secrets pair.
    ///
    /// Behavior:
    /// - Iterate `config.layers` and open a [`GdalSource`] for each source
    ///   URI referenced by any layer. URIs are deduplicated so opening the
    ///   same dataset twice never happens, even when several layers share
    ///   a source.
    /// - Build a [`LayeredCache`] from `config.cache` + `secrets.credentials`.
    /// - Wrap everything in `Arc`s so the state is cheap to clone into
    ///   handlers via `axum::extract::State`.
    pub fn new(config: Config, secrets: Secrets) -> Result<Self> {
        let mut registry = SourceRegistry::new();
        let mut seen: HashSet<String> = HashSet::new();
        for layer in &config.layers {
            for source_cfg in &layer.sources {
                if !seen.insert(source_cfg.uri.clone()) {
                    continue;
                }
                let src = GdalSource::open(source_cfg).with_context(|| {
                    format!(
                        "opening source `{uri}` for layer `{layer}`",
                        uri = source_cfg.uri,
                        layer = layer.name
                    )
                })?;
                registry.insert(source_cfg.uri.clone(), Arc::new(src) as Arc<dyn Source>);
            }
        }

        let source_count = seen.len();

        let cache = LayeredCache::new(&config.cache, &secrets.credentials)
            .context("building layered cache")?;

        Self::from_parts(config, registry, cache, source_count)
    }

    /// Assemble state from already-built parts. The render pool is sized
    /// from `config.server`; the request [`Catalog`] is built from `config`
    /// (failing if GDAL rejects a configured CRS). Used by [`AppState::new`]
    /// and by tests that supply their own registry/cache.
    pub fn from_parts(
        config: Config,
        sources: SourceRegistry,
        cache: LayeredCache,
        source_count: usize,
    ) -> Result<Self> {
        // Before the coverage transforms below (and any request).
        crate::crs::disable_proj_network()?;
        let catalog = Catalog::build(&config).context("building request catalog")?;
        let cpu = CpuPool::new(
            config.server.effective_render_concurrency(),
            config.server.render_queue_timeout,
        );
        // Every (source, grid) a layer can be rendered in: coverage is
        // computed and the source-read budget checked now, so request
        // threads never run GDAL/PROJ for it and no tile can be expensive.
        let serves: Vec<_> = catalog
            .layers()
            .iter()
            .filter_map(|l| l.config().sources.first().map(|s| (l, s.uri.clone())))
            .flat_map(|(l, uri)| l.grids().iter().map(move |g| (uri.clone(), g.grid().clone())))
            .collect();
        let renderer = Renderer::new(Arc::new(sources), Arc::new(cache), cpu, serves)
            .context("checking sources against the layers' grids")?;
        let documents = Documents::build(&config).context("rendering capabilities documents")?;
        Ok(Self {
            config: Arc::new(config),
            catalog: Arc::new(catalog),
            renderer: Arc::new(renderer),
            documents: Arc::new(documents),
            source_count,
        })
    }
}

/// Server-relative base URLs used in capabilities documents (v1 has no
/// configurable public URL).
pub const WMS_BASE_URL: &str = "/wms";
pub const WMTS_BASE_URL: &str = "";
pub const TMS_BASE_URL: &str = "";

/// Every capabilities/metadata document, rendered once from the config at
/// startup. They depend on nothing but the config, so request handlers just
/// return the bytes — no GDAL/PROJ work (CRS transforms) on async threads,
/// and a GetCapabilities flood costs no CPU. A document GDAL can't build is
/// a startup error, never a silently incomplete document.
pub struct Documents {
    pub wms_111: String,
    pub wms_130: String,
    pub wmts: String,
    pub tms_root: String,
    pub tms_service: String,
    pub tms_layers: HashMap<String, String>,
}

impl Documents {
    pub fn build(config: &Config) -> Result<Self> {
        use crate::protocol::{tms, wms, wmts};
        let mut tms_layers = HashMap::new();
        for layer in &config.layers {
            let doc = tms::capabilities::generate_tile_map(layer, config, TMS_BASE_URL)
                .with_context(|| format!("TMS TileMap for layer `{}`", layer.name))?;
            tms_layers.insert(layer.name.clone(), doc);
        }
        Ok(Self {
            wms_111: wms::capabilities::generate(config, wms::request::WmsVersion::V1_1_1, WMS_BASE_URL)
                .context("WMS 1.1.1 capabilities")?,
            wms_130: wms::capabilities::generate(config, wms::request::WmsVersion::V1_3_0, WMS_BASE_URL)
                .context("WMS 1.3.0 capabilities")?,
            wmts: wmts::capabilities::generate(config, WMTS_BASE_URL).context("WMTS capabilities")?,
            tms_root: tms::capabilities::generate_root(TMS_BASE_URL),
            tms_service: tms::capabilities::generate_service_metadata(config, TMS_BASE_URL)
                .context("TMS service metadata")?,
            tms_layers,
        })
    }
}
