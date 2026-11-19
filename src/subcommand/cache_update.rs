//! `kwms cache update` — fill the durable cache tiers ahead of requests.
//!
//! Which levels? By default (`--max-level auto`) the layer's `cache.max_level`,
//! which defaults to every level whose tiles are
//! *larger than one source file*. Rendering such a tile opens several source
//! files (a country-wide tile of the 2026 ortho opens all 498 remote COGs,
//! seconds per tile), so it is worth caching. A tile smaller than a file
//! opens one to four, each through that file's own overviews, which is
//! about as cheap as reading a cached tile: from there on you might as well
//! use the source. See [`crate::source::gdal::footprint`].
//!
//! Tiles go through the same key, rendering and "no empty tiles" rule as
//! serving ([`seed_grid_tile`]), so WMTS, TMS and WMS requests all hit
//! them. Fresh tiles are skipped unless `--force`.
//!
//! Timeouts are relaxed compared to serving: the server's request timeout
//! does not apply, each tile gets `--tile-timeout` (default 10 min) per
//! attempt, "busy" waits for a dataset or render slot are retried without
//! counting as failures, and other errors (network, storage) are retried
//! `--retries` times with backoff. A tile that still fails is reported at
//! the end; the run carries on and exits non-zero.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tokio::task::JoinSet;

use crate::cache::{grid_dir, policy::resolve_mode};
use crate::config::{CacheMode, LayerConfig};
use crate::encode::OutputFormat;
use crate::pipeline::error::{classify, Disposition};
use crate::pipeline::render::{seed_grid_tile, SeedOutcome};
use crate::pipeline::wms_assemble::{tile_range_for_bbox, TileRange};
use crate::server::AppState;
use crate::tile::TileGrid;

/// Deepest level to fill.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MaxLevel {
    /// The layer's `cache.max_level` (by default: levels whose tiles are
    /// larger than one source file).
    Auto,
    Level(u32),
}

impl std::str::FromStr for MaxLevel {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        if s.eq_ignore_ascii_case("auto") {
            return Ok(Self::Auto);
        }
        s.parse().map(Self::Level).map_err(|_| format!("expected `auto` or a level number, got `{s}`"))
    }
}

pub struct Args {
    pub config: PathBuf,
    pub secrets: PathBuf,
    /// Layer names; empty = all.
    pub layers: Vec<String>,
    /// Grid names (`rd_new_quad`) or WMTS TileMatrixSet names; empty = all of the layer's.
    pub grids: Vec<String>,
    /// Formats (`jpeg`, `png`, …); empty = all of the layer's.
    pub formats: Vec<String>,
    pub min_level: u32,
    pub max_level: MaxLevel,
    pub force: bool,
    pub dry_run: bool,
    pub concurrency: usize,
    pub tile_timeout: Duration,
    pub retries: u32,
    pub max_tiles: u64,
}

/// One `(layer, grid, format)` to fill, with its levels.
struct Job {
    layer: Arc<LayerConfig>,
    grid: Arc<dyn TileGrid>,
    format: OutputFormat,
    levels: Vec<TileRange>,
    /// Why these levels (for the plan printout).
    reason: String,
}

pub async fn run(args: Args) -> Result<()> {
    init_tracing();
    crate::crs::disable_proj_network()?;
    if args.concurrency == 0 {
        bail!("--concurrency must be at least 1");
    }
    let (config, secrets) = crate::config::load::load(&args.config, &args.secrets)
        .with_context(|| format!("loading config (`{}`) and secrets (`{}`)", args.config.display(), args.secrets.display()))?;
    let state = Arc::new(AppState::new(config, secrets).context("opening sources and cache")?);
    let cache = &state.renderer.cache;
    if !cache.has_durable_tier() {
        bail!("the cache has no durable tier (only {:?}); there is nothing to update", cache.describe());
    }

    let jobs = plan(&state, &args)?;
    let total: u64 = jobs.iter().flat_map(|j| &j.levels).map(|r| r.count).sum();
    println!("cache tiers: {}", cache.describe().join(" -> "));
    for job in &jobs {
        let (lo, hi) = (job.levels.first().map(|r| r.level), job.levels.last().map(|r| r.level));
        let levels = match (lo, hi) {
            (Some(lo), Some(hi)) => format!("levels {lo}-{hi}"),
            _ => "no levels".into(),
        };
        let count: u64 = job.levels.iter().map(|r| r.count).sum();
        println!("  {}  {levels}  {count} tiles  ({})", job_name(job), job.reason);
    }
    println!("total: {total} tiles");
    if total > args.max_tiles {
        bail!("{total} tiles exceeds --max-tiles {}; narrow the levels or raise the limit", args.max_tiles);
    }
    if args.dry_run || total == 0 {
        return Ok(());
    }
    cache.check().await.context("cache is not usable")?;

    let started = Instant::now();
    let mut failures: Vec<String> = Vec::new();
    let mut sums = Counts::default();
    for job in &jobs {
        for range in &job.levels {
            let counts = run_level(&state, job, *range, &args, &mut failures).await;
            println!(
                "  {} level {}: {} tiles: {} stored, {} fresh, {} empty, {} outside, {} failed ({:.1}s)",
                job_name(job),
                range.level,
                range.count,
                counts.stored,
                counts.fresh,
                counts.empty,
                counts.outside,
                counts.failed,
                counts.elapsed.as_secs_f64()
            );
            sums.add(&counts);
        }
    }
    println!(
        "done in {:.0}s: {} stored, {} fresh, {} empty, {} outside, {} failed",
        started.elapsed().as_secs_f64(),
        sums.stored,
        sums.fresh,
        sums.empty,
        sums.outside,
        sums.failed
    );
    if !failures.is_empty() {
        for f in failures.iter().take(10) {
            eprintln!("  failed: {f}");
        }
        if failures.len() > 10 {
            eprintln!("  … and {} more", failures.len().saturating_sub(10));
        }
        bail!("{} tile(s) failed; run the same command again to retry just those", failures.len());
    }
    Ok(())
}

fn job_name(job: &Job) -> String {
    let fmt = job.format.extension();
    format!("{}/{}/{fmt}", job.layer.name, grid_dir(job.grid.name(), job.grid.crs()))
}

/// Work out every `(layer, grid, format)` and its levels.
fn plan(state: &AppState, args: &Args) -> Result<Vec<Job>> {
    for name in &args.layers {
        if state.catalog.layer(name).is_none() {
            bail!("no layer named '{name}'");
        }
    }
    let mut jobs = Vec::new();
    for cl in state.catalog.layers() {
        let layer = cl.config();
        if !args.layers.is_empty() && !args.layers.contains(&layer.name) {
            continue;
        }
        let source = layer.sources.first().ok_or_else(|| anyhow!("layer '{}' has no sources", layer.name))?;
        let formats = select_formats(layer, &args.formats)?;
        let layer_arc = Arc::new(layer.clone());
        for cg in cl.grids() {
            if !args.grids.is_empty() && !args.grids.iter().any(|g| g == cg.name() || cg.tile_matrix_sets().contains(g)) {
                continue;
            }
            let grid = cg.grid().clone();
            let coverage = state.renderer.source_coverage(&source.uri, grid.crs())?;
            // The layer's durable depth (`cache.max_level`) is the limit:
            // the server never reads deeper tiles from the durable tiers.
            let (limit, why) = state
                .renderer
                .cache_limit(&layer.name, grid.name())
                .cloned()
                .ok_or_else(|| anyhow!("no cache limit computed for layer '{}' grid '{}'", layer.name, grid.name()))?;
            let (max, reason) = match (args.max_level, limit) {
                (MaxLevel::Auto, _) => (limit, why),
                (MaxLevel::Level(n), Some(l)) if n > l => {
                    (limit, format!("--max-level {n} capped to the layer's durable depth {l} ({why})"))
                }
                (MaxLevel::Level(n), Some(_)) => (Some(n), format!("--max-level {n}")),
                (MaxLevel::Level(n), None) => (None, format!("--max-level {n}, but the layer stores no level durably ({why})")),
            };
            let mut levels = Vec::new();
            let mut passthrough = Vec::new();
            if let Some(max) = max {
                for level in args.min_level..=max {
                    if resolve_mode(&layer.cache, level) == CacheMode::Passthrough {
                        passthrough.push(level);
                        continue;
                    }
                    if let Some(range) = tile_range_for_bbox(grid.as_ref(), level, &coverage)? {
                        levels.push(range);
                    }
                }
            }
            let reason = if passthrough.is_empty() {
                reason
            } else {
                format!("{reason}; skipping passthrough levels {passthrough:?}")
            };
            for &format in &formats {
                jobs.push(Job { layer: layer_arc.clone(), grid: grid.clone(), format, levels: levels.clone(), reason: reason.clone() });
            }
        }
    }
    Ok(jobs)
}

fn select_formats(layer: &LayerConfig, wanted: &[String]) -> Result<Vec<OutputFormat>> {
    if wanted.is_empty() {
        return Ok(layer.formats.clone());
    }
    wanted
        .iter()
        .map(|w| {
            let w = if w.eq_ignore_ascii_case("jpg") { "jpeg" } else { w.as_str() };
            layer
                .formats
                .iter()
                .copied()
                .find(|f| f.extension().eq_ignore_ascii_case(w) || format!("{f:?}").eq_ignore_ascii_case(w))
                .ok_or_else(|| anyhow!("layer '{}' does not offer format '{w}'", layer.name))
        })
        .collect()
}

#[derive(Default)]
struct Counts {
    stored: u64,
    fresh: u64,
    empty: u64,
    outside: u64,
    failed: u64,
    elapsed: Duration,
}

impl Counts {
    fn add(&mut self, o: &Counts) {
        self.stored = self.stored.saturating_add(o.stored);
        self.fresh = self.fresh.saturating_add(o.fresh);
        self.empty = self.empty.saturating_add(o.empty);
        self.outside = self.outside.saturating_add(o.outside);
        self.failed = self.failed.saturating_add(o.failed);
    }
}

/// Fill every tile of one level, `--concurrency` at a time.
async fn run_level(state: &Arc<AppState>, job: &Job, range: TileRange, args: &Args, failures: &mut Vec<String>) -> Counts {
    let started = Instant::now();
    let mut counts = Counts::default();
    let mut set = JoinSet::new();
    let mut coords = range.coords();
    let mut done: u64 = 0;
    let mut last_report = Instant::now();
    loop {
        while set.len() < args.concurrency {
            let Some(coord) = coords.next() else { break };
            let (state, layer, grid) = (state.clone(), job.layer.clone(), job.grid.clone());
            let (format, force, timeout, retries) = (job.format, args.force, args.tile_timeout, args.retries);
            set.spawn(async move {
                let out = seed_with_retries(&state, &layer, &grid, coord, format, force, timeout, retries).await;
                (coord, out)
            });
        }
        let Some(joined) = set.join_next().await else { break };
        done = done.saturating_add(1);
        match joined {
            Ok((_, Ok(SeedOutcome::Stored))) => counts.stored = counts.stored.saturating_add(1),
            Ok((_, Ok(SeedOutcome::Fresh))) => counts.fresh = counts.fresh.saturating_add(1),
            Ok((_, Ok(SeedOutcome::Empty))) => counts.empty = counts.empty.saturating_add(1),
            Ok((_, Ok(SeedOutcome::Outside))) => counts.outside = counts.outside.saturating_add(1),
            Ok((coord, Err(e))) => {
                counts.failed = counts.failed.saturating_add(1);
                failures.push(format!("{} {}/{}/{}: {e:#}", job_name(job), coord.z, coord.x, coord.y));
            }
            Err(e) => {
                counts.failed = counts.failed.saturating_add(1);
                failures.push(format!("{} level {}: task failed: {e}", job_name(job), range.level));
            }
        }
        if last_report.elapsed() >= Duration::from_secs(30) {
            println!("    … {} level {}: {done}/{}", job_name(job), range.level, range.count);
            last_report = Instant::now();
        }
    }
    counts.elapsed = started.elapsed();
    counts
}

/// One tile, with relaxed timeouts: each attempt may take `timeout`;
/// "busy" (no free dataset/render slot) waits and tries again without
/// using up a retry; other errors are retried `retries` times with
/// exponential backoff (2 s, 4 s, 8 s, … capped at 60 s).
#[allow(clippy::too_many_arguments)]
async fn seed_with_retries(
    state: &AppState,
    layer: &LayerConfig,
    grid: &Arc<dyn TileGrid>,
    coord: crate::tile::TileCoord,
    format: OutputFormat,
    force: bool,
    timeout: Duration,
    retries: u32,
) -> Result<SeedOutcome> {
    let mut failed_attempts = 0u32;
    let mut busy_waits = 0u32;
    loop {
        let attempt = tokio::time::timeout(timeout, seed_grid_tile(&state.renderer, layer, grid, coord, format, force)).await;
        let err = match attempt {
            Ok(Ok(outcome)) => return Ok(outcome),
            Ok(Err(e)) if classify(&e) == Disposition::Busy && busy_waits < 600 => {
                busy_waits = busy_waits.saturating_add(1);
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            Ok(Err(e)) => e,
            Err(_) => anyhow!("timed out after {}s", timeout.as_secs()),
        };
        if failed_attempts >= retries {
            return Err(err.context(format!("after {} attempt(s)", failed_attempts.saturating_add(1))));
        }
        let backoff = Duration::from_secs((2u64 << failed_attempts.min(5)).min(60));
        tracing::warn!(
            tile = %format_args!("{}/{}/{}", coord.z, coord.x, coord.y),
            error = %format_args!("{err:#}"),
            retry_in_s = backoff.as_secs(),
            "tile failed; retrying"
        );
        failed_attempts = failed_attempts.saturating_add(1);
        tokio::time::sleep(backoff).await;
    }
}

/// Human-readable warnings on stderr (the progress report goes to stdout).
fn init_tracing() {
    use tracing_subscriber::{fmt, EnvFilter};
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));
    let _ = fmt().with_env_filter(filter).with_writer(std::io::stderr).try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn max_level_parses() {
        assert_eq!("auto".parse::<MaxLevel>(), Ok(MaxLevel::Auto));
        assert_eq!("7".parse::<MaxLevel>(), Ok(MaxLevel::Level(7)));
        assert!("deep".parse::<MaxLevel>().is_err());
    }
}
