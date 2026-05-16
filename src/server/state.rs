//! Application state shared by all axum handlers.

use std::collections::HashSet;
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::cache::LayeredCache;
use crate::config::{Config, Secrets};
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
    pub sources: Arc<SourceRegistry>,
    pub cache: Arc<LayeredCache>,
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

        Ok(Self {
            config: Arc::new(config),
            sources: Arc::new(registry),
            cache: Arc::new(cache),
            source_count,
        })
    }

    /// Test-only constructor — accept pre-built parts. Lets tests build an
    /// `AppState` without going through the GDAL `open` path (which requires
    /// real datasets on disk) or the persistent-cache builder (which fails
    /// for the default `Fs` backend without the `services-fs` feature).
    #[cfg(test)]
    pub fn for_tests(
        config: Config,
        sources: SourceRegistry,
        cache: LayeredCache,
        source_count: usize,
    ) -> Self {
        Self {
            config: Arc::new(config),
            sources: Arc::new(sources),
            cache: Arc::new(cache),
            source_count,
        }
    }
}
