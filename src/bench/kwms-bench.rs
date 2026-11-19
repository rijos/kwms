//! kwms-bench — benchmark a WMTS/WMS server the way a desktop client (QGIS)
//! uses it.
//!
//! A *view* is one screen of map: for WMTS every tile covering a WxH pixel
//! viewport at one zoom level, fetched with N parallel connections (QGIS uses
//! about 6 per host); for WMS one GetMap of WxH pixels. Each run first loads
//! `--views` new views (cold: rendered by the server unless already cached),
//! then the same views again (warm: served from the cache).
//!
//! View centres are sampled over actual data: random points inside the
//! footprints of a tile index shapefile (`--index`), so no time is spent on
//! empty sea tiles. The WMTS tile grid and URL template come from the
//! server's GetCapabilities, so the same tool can bench any WMTS server, e.g.
//! kwms against an existing service.
//!
//! Against kwms it also reports, per phase, where server time went
//! (`kwms_stage_seconds`, `kwms_cache_seconds` from `--metrics`) and, with
//! `--pid` on the same Linux host, the server's CPU time per thread name.
//! GDAL's worker threads inherit the `gdal-pool` name, so that group covers
//! all GDAL work: compare it with `gdal_run` wall time to see how much of a
//! read is waiting on the network.
//!
//! Build and run:
//!
//! ```sh
//! cargo run --release --features bindgen,bench --bin kwms-bench -- \
//!     --url http://127.0.0.1:8080 wmts --level 14
//! cargo run --release --features bindgen,bench --bin kwms-bench -- \
//!     --url http://127.0.0.1:8080 --pid "$(pgrep -x kwms)" wms --res 0.25
//! ```

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Parser, Subcommand};
use kwms::crs::{transform_coord, Coord, Crs};
use serde::Serialize;
use serde_json::{json, Value};

/// OGC standardized rendering pixel size (m), to turn a WMTS
/// ScaleDenominator into CRS units per pixel (projected CRSes in metres).
const METERS_PER_PIXEL_AT_SCALE_1: f64 = 0.00028;
const WMTS_NS: &str = "http://www.opengis.net/wmts/1.0";
const OWS_NS: &str = "http://www.opengis.net/ows/1.1";

#[derive(Parser, Serialize)]
#[command(name = "kwms-bench", about = "Benchmark a WMTS/WMS server the way QGIS uses it")]
struct Args {
    /// Server base URL.
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    url: String,
    #[arg(long, default_value = "2026_orthoHR")]
    layer: String,
    /// Shapefile whose polygons hold the data (view centres are sampled inside).
    #[arg(long, default_value = "data/2026_orthoHR/tiles.shp")]
    index: PathBuf,
    /// CRS of the index.
    #[arg(long, default_value = "EPSG:28992")]
    index_crs: String,
    /// Views per phase.
    #[arg(long, default_value_t = 5)]
    views: usize,
    /// Viewport in pixels, WxH.
    #[arg(long, default_value = "1920x1080")]
    size: String,
    /// Parallel connections (QGIS: ~6).
    #[arg(long, default_value_t = 6)]
    concurrency: usize,
    /// Random seed (default: time, so new views each run).
    #[arg(long)]
    seed: Option<u64>,
    /// Per-request timeout in seconds.
    #[arg(long, default_value_t = 120.0)]
    timeout: f64,
    /// kwms /metrics URL ('' to skip).
    #[arg(long, default_value = "http://127.0.0.1:9091/metrics")]
    metrics: String,
    /// Skip the warm (repeat) phase.
    #[arg(long)]
    no_warm: bool,
    /// Server process id: report its CPU time per thread (Linux, same host).
    #[arg(long)]
    pid: Option<u32>,
    /// Append the results as one JSON line to this file.
    #[arg(long)]
    json: Option<PathBuf>,
    /// Free text stored with --json results.
    #[arg(long, default_value = "")]
    label: String,
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Subcommand, Serialize)]
enum Mode {
    /// Tile viewports.
    Wmts {
        #[arg(long, default_value = "NetherlandsRDNewQuad")]
        matrix_set: String,
        #[arg(long)]
        level: u32,
        #[arg(long, default_value = "jpeg")]
        format: String,
    },
    /// GetMap viewports.
    Wms {
        #[arg(long, default_value = "EPSG:28992")]
        crs: String,
        /// Metres (CRS units) per pixel.
        #[arg(long)]
        res: f64,
        #[arg(long, default_value = "image/jpeg")]
        format: String,
    },
}

// ------------------------------------------------------------------ sampling

/// Bounding boxes (minx, miny, maxx, maxy) of every non-null record in a
/// `.shp` file.
fn shapefile_boxes(path: &Path) -> Result<Vec<[f64; 4]>> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut boxes = Vec::new();
    let mut at = 100usize; // file header
    while let Some(head) = bytes.get(at..at.saturating_add(8)) {
        // Record header: number and content length in 16-bit words, big-endian.
        let words = i32::from_be_bytes([head[4], head[5], head[6], head[7]]);
        let len = usize::try_from(words)
            .context("negative shapefile record length")?
            .saturating_mul(2);
        let start = at.saturating_add(8);
        let content = bytes
            .get(start..start.saturating_add(len))
            .context("truncated shapefile record")?;
        if content.len() >= 36 && i32::from_le_bytes([content[0], content[1], content[2], content[3]]) != 0 {
            let f = |i: usize| f64::from_le_bytes(content[4 + 8 * i..12 + 8 * i].try_into().unwrap_or([0; 8]));
            boxes.push([f(0), f(1), f(2), f(3)]);
        }
        at = start.saturating_add(len);
    }
    if boxes.is_empty() {
        bail!("{} has no shapes", path.display());
    }
    Ok(boxes)
}

/// SplitMix64: a small, good-enough PRNG so `--seed` reproduces views.
struct Rng(u64);

impl Rng {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    /// Uniform in [0, 1).
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn range(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.unit()
    }
}

/// `n` random points inside random boxes, kept away from box edges.
fn sample_points(boxes: &[[f64; 4]], n: usize, rng: &mut Rng) -> Vec<(f64, f64)> {
    (0..n)
        .map(|_| {
            let i = (rng.next_u64() % boxes.len() as u64) as usize;
            let [minx, miny, maxx, maxy] = boxes[i];
            let (mx, my) = ((maxx - minx) * 0.15, (maxy - miny) * 0.15);
            (rng.range(minx + mx, maxx - mx), rng.range(miny + my, maxy - my))
        })
        .collect()
}

fn reproject(points: &[(f64, f64)], from: &str, to: &str) -> Result<Vec<(f64, f64)>> {
    if from == to {
        return Ok(points.to_vec());
    }
    let (from, to) = (Crs::new(from), Crs::new(to));
    points
        .iter()
        .map(|&(x, y)| transform_coord(Coord { x, y }, &from, &to).map(|c| (c.x, c.y)))
        .collect()
}

// ------------------------------------------------------------------ http

/// One request: seconds, status (or error kind), bytes.
struct Sample {
    secs: f64,
    status: String,
    bytes: usize,
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .into()
}

fn fetch(agent: &ureq::Agent, url: &str) -> Sample {
    let started = Instant::now();
    let (status, bytes) = match agent.get(url).call() {
        Ok(mut resp) => {
            let status = resp.status().as_u16().to_string();
            match resp.body_mut().with_config().limit(256 << 20).read_to_vec() {
                Ok(body) => (status, body.len()),
                Err(e) => (format!("error: body ({e})"), 0),
            }
        }
        Err(e) => (format!("error: {}", error_kind(&e)), 0),
    };
    Sample {
        secs: started.elapsed().as_secs_f64(),
        status,
        bytes,
    }
}

fn error_kind(e: &ureq::Error) -> &'static str {
    match e {
        ureq::Error::Timeout(_) => "timeout",
        ureq::Error::Io(_) => "io",
        ureq::Error::ConnectionFailed => "connection failed",
        _ => "other",
    }
}

fn get_text(agent: &ureq::Agent, url: &str) -> Result<String> {
    let mut resp = agent.get(url).call().with_context(|| format!("GET {url}"))?;
    if resp.status().as_u16() != 200 {
        bail!("GET {url}: HTTP {}", resp.status());
    }
    resp.body_mut()
        .with_config()
        .limit(64 << 20)
        .read_to_string()
        .with_context(|| format!("reading {url}"))
}

/// Cache counters and stage timings from a kwms /metrics endpoint (empty if
/// unavailable or not kwms).
fn metrics(agent: &ureq::Agent, url: &str) -> BTreeMap<String, f64> {
    const KEEP: [&str; 7] = [
        "kwms_cache_hits_total",
        "kwms_cache_misses_total",
        "kwms_cache_errors_total",
        "kwms_stage_seconds_sum",
        "kwms_stage_seconds_count",
        "kwms_cache_seconds_sum",
        "kwms_cache_seconds_count",
    ];
    if url.is_empty() {
        return BTreeMap::new();
    }
    let Ok(text) = get_text(agent, url) else {
        return BTreeMap::new();
    };
    text.lines()
        .filter(|l| KEEP.iter().any(|k| l.starts_with(k)))
        .filter_map(|l| {
            let (name, value) = l.rsplit_once(' ')?;
            Some((name.to_string(), value.parse().ok()?))
        })
        .collect()
}

/// CPU seconds per thread name of process `pid` (Linux /proc); trailing
/// numbers (`gdal-pool[3]`, `worker-12`) are folded into one group.
fn cpu_by_thread(pid: Option<u32>) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    let Some(pid) = pid else { return out };
    // USER_HZ: 100 on every Linux kwms runs on.
    let tick = 100.0;
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return out;
    };
    for task in tasks.flatten() {
        let Ok(stat) = std::fs::read_to_string(task.path().join("stat")) else {
            continue;
        };
        let (Some(open), Some(close)) = (stat.find('('), stat.rfind(')')) else {
            continue;
        };
        let name = &stat[open + 1..close];
        let fields: Vec<&str> = stat[close + 1..].split_whitespace().collect();
        // utime, stime: fields 14 and 15 of stat = 11 and 12 after the name.
        let ticks = |i: usize| fields.get(i).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0);
        let group = name
            .trim_end_matches(|c: char| c.is_ascii_digit() || c == '[' || c == ']')
            .trim_end_matches(['-', '_']);
        let group = if group.is_empty() { name } else { group };
        *out.entry(group.to_string()).or_default() += (ticks(11) + ticks(12)) / tick;
    }
    out
}

/// `{stage: (count, total seconds)}` from kwms_stage_seconds /
/// kwms_cache_seconds deltas.
fn stage_table(before: &BTreeMap<String, f64>, after: &BTreeMap<String, f64>) -> BTreeMap<String, (u64, f64)> {
    let label = |labels: &str, key: &str| {
        let needle = format!("{key}=\"");
        let start = labels.find(&needle)? + needle.len();
        Some(labels[start..].split('"').next()?.to_string())
    };
    let mut rows = BTreeMap::new();
    for (k, v) in after {
        let Some(labels) = k
            .strip_prefix("kwms_stage_seconds_sum{")
            .or_else(|| k.strip_prefix("kwms_cache_seconds_sum{"))
        else {
            continue;
        };
        let name = match label(labels, "stage") {
            Some(stage) => stage,
            None => format!(
                "cache_{}[{}]",
                label(labels, "op").unwrap_or_default(),
                label(labels, "tier").unwrap_or_default()
            ),
        };
        let count_key = k.replacen("_sum{", "_count{", 1);
        let n = after.get(&count_key).copied().unwrap_or(0.0) - before.get(&count_key).copied().unwrap_or(0.0);
        let secs = v - before.get(k).copied().unwrap_or(0.0);
        if n > 0.0 {
            rows.insert(name, (n as u64, secs));
        }
    }
    rows
}

// ------------------------------------------------------------------ WMTS

struct Grid {
    crs: String,
    res: f64,
    origin: (f64, f64),
    tile: (u32, u32),
    matrix: (u32, u32),
    template: String,
}

fn wmts_grid(agent: &ureq::Agent, base: &str, layer: &str, matrix_set: &str, level: u32, format: &str) -> Result<Grid> {
    let xml = get_text(agent, &format!("{base}/wmts?SERVICE=WMTS&REQUEST=GetCapabilities"))?;
    let doc = roxmltree::Document::parse(&xml).context("parsing WMTS capabilities")?;
    let is =
        |n: &roxmltree::Node, ns: &str, name: &str| n.tag_name().namespace() == Some(ns) && n.tag_name().name() == name;
    let child_text = |n: roxmltree::Node, ns: &str, name: &str| -> Option<String> {
        n.children()
            .find(|c| is(c, ns, name))
            .and_then(|c| c.text())
            .map(|t| t.trim().to_string())
    };
    let contents = doc
        .descendants()
        .find(|n| is(n, WMTS_NS, "Contents"))
        .context("no Contents")?;
    let tms = contents
        .children()
        .filter(|n| is(n, WMTS_NS, "TileMatrixSet"))
        .find(|n| child_text(*n, OWS_NS, "Identifier").as_deref() == Some(matrix_set))
        .with_context(|| format!("no TileMatrixSet '{matrix_set}'"))?;
    let supported = child_text(tms, OWS_NS, "SupportedCRS").context("no SupportedCRS")?;
    let code = supported.rsplit(':').next().unwrap_or_default();
    let tm = tms
        .children()
        .filter(|n| is(n, WMTS_NS, "TileMatrix"))
        .find(|n| child_text(*n, OWS_NS, "Identifier").as_deref() == Some(&level.to_string()))
        .with_context(|| format!("no TileMatrix {level} in '{matrix_set}'"))?;
    let num = |name: &str| -> Result<f64> {
        child_text(tm, WMTS_NS, name)
            .with_context(|| format!("no {name}"))?
            .parse()
            .with_context(|| format!("bad {name}"))
    };
    let corner = child_text(tm, WMTS_NS, "TopLeftCorner").context("no TopLeftCorner")?;
    let mut xy = corner.split_whitespace().map(str::parse::<f64>);
    let origin = match (xy.next(), xy.next()) {
        (Some(Ok(x)), Some(Ok(y))) => (x, y),
        _ => bail!("bad TopLeftCorner '{corner}'"),
    };
    let lyr = contents
        .children()
        .filter(|n| is(n, WMTS_NS, "Layer"))
        .find(|n| child_text(*n, OWS_NS, "Identifier").as_deref() == Some(layer))
        .with_context(|| format!("no layer '{layer}'"))?;
    let template = lyr
        .children()
        .filter(|n| is(n, WMTS_NS, "ResourceURL"))
        .find(|n| n.attribute("format").is_some_and(|f| f.ends_with(format)))
        .and_then(|n| n.attribute("template"))
        .with_context(|| format!("no ResourceURL for format '{format}'"))?;
    Ok(Grid {
        crs: format!("EPSG:{code}"),
        res: num("ScaleDenominator")? * METERS_PER_PIXEL_AT_SCALE_1,
        origin,
        tile: (num("TileWidth")? as u32, num("TileHeight")? as u32),
        matrix: (num("MatrixWidth")? as u32, num("MatrixHeight")? as u32),
        template: template.to_string(),
    })
}

/// Tile (col, row) pairs covering a `size` pixel viewport at `centre`.
fn view_tiles(grid: &Grid, (cx, cy): (f64, f64), size: (u32, u32)) -> Vec<(u32, u32)> {
    let ((x0, y0), res) = (grid.origin, grid.res);
    let (tw, th) = (f64::from(grid.tile.0) * res, f64::from(grid.tile.1) * res);
    let (w, h) = (f64::from(size.0) * res, f64::from(size.1) * res);
    let clamp = |v: f64, max: u32| v.floor().clamp(0.0, f64::from(max.saturating_sub(1))) as u32;
    let (c0, c1) = (
        clamp((cx - w / 2.0 - x0) / tw, grid.matrix.0),
        clamp((cx + w / 2.0 - x0) / tw, grid.matrix.0),
    );
    let (r0, r1) = (
        clamp((y0 - (cy + h / 2.0)) / th, grid.matrix.1),
        clamp((y0 - (cy - h / 2.0)) / th, grid.matrix.1),
    );
    (r0..=r1).flat_map(|r| (c0..=c1).map(move |c| (c, r))).collect()
}

fn tile_url(grid: &Grid, matrix_set: &str, level: u32, (col, row): (u32, u32)) -> String {
    grid.template
        .replace("{TileMatrixSet}", matrix_set)
        .replace("{TileMatrix}", &level.to_string())
        .replace("{TileCol}", &col.to_string())
        .replace("{TileRow}", &row.to_string())
}

// ------------------------------------------------------------------ running

/// Load each view in turn, its URLs `concurrency` at a time. Returns every
/// request and the time per view.
fn run_views(agent: &ureq::Agent, views: &[Vec<String>], concurrency: usize) -> (Vec<Sample>, Vec<f64>) {
    let mut samples = Vec::new();
    let mut view_times = Vec::with_capacity(views.len());
    for urls in views {
        let started = Instant::now();
        let next = AtomicUsize::new(0);
        let results = Mutex::new(Vec::with_capacity(urls.len()));
        std::thread::scope(|s| {
            for _ in 0..concurrency.min(urls.len()).max(1) {
                s.spawn(|| {
                    while let Some(url) = urls.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let sample = fetch(agent, url);
                        if let Ok(mut r) = results.lock() {
                            r.push(sample);
                        }
                    }
                });
            }
        });
        view_times.push(started.elapsed().as_secs_f64());
        samples.extend(results.into_inner().unwrap_or_default());
    }
    (samples, view_times)
}

fn pct(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }
    let i = (p / 100.0 * (sorted.len() - 1) as f64).round() as usize;
    sorted[i.min(sorted.len() - 1)]
}

#[allow(clippy::too_many_arguments)]
fn summarize(
    phase: &str,
    samples: &[Sample],
    view_times: &[f64],
    before: &BTreeMap<String, f64>,
    after: &BTreeMap<String, f64>,
    cpu_before: &BTreeMap<String, f64>,
    cpu_after: &BTreeMap<String, f64>,
) -> Value {
    let mut lat: Vec<f64> = samples.iter().map(|s| s.secs).collect();
    lat.sort_by(f64::total_cmp);
    let mut statuses: BTreeMap<&str, usize> = BTreeMap::new();
    for s in samples {
        *statuses.entry(s.status.as_str()).or_default() += 1;
    }
    let bytes: usize = samples.iter().map(|s| s.bytes).sum();
    let wall: f64 = view_times.iter().sum();
    let ms = |v: f64| (v * 1000.0).round();
    let view_mean = if view_times.is_empty() {
        f64::NAN
    } else {
        wall / view_times.len() as f64
    };
    let view_max = view_times.iter().copied().fold(0.0, f64::max);
    let req_per_s = if wall > 0.0 { lat.len() as f64 / wall } else { 0.0 };
    let cache: BTreeMap<String, f64> = after
        .iter()
        .filter(|(k, _)| !k.contains("_seconds_"))
        .map(|(k, v)| {
            (
                k.replacen("kwms_cache_", "", 1),
                v - before.get(k).copied().unwrap_or(0.0),
            )
        })
        .filter(|(_, d)| *d != 0.0)
        .collect();
    let stages = stage_table(before, after);
    let mut cpu: Vec<(String, f64)> = cpu_after
        .iter()
        .map(|(g, v)| (g.clone(), v - cpu_before.get(g).copied().unwrap_or(0.0)))
        .filter(|(_, v)| *v > 0.0)
        .collect();
    cpu.sort_by(|a, b| b.1.total_cmp(&a.1));

    println!(
        "{phase:5} {:3} views {:5} req  req p50 {:6} ms  p95 {:6} ms  max {:6} ms  view mean {:6.2} s  max {:6.2} s  {:6.1} req/s  status {statuses:?}",
        view_times.len(),
        lat.len(),
        ms(pct(&lat, 50.0)),
        ms(pct(&lat, 95.0)),
        ms(lat.last().copied().unwrap_or(f64::NAN)),
        view_mean,
        view_max,
        req_per_s,
    );
    if !cache.is_empty() {
        println!("      cache: {cache:?}");
    }
    if !stages.is_empty() {
        println!("      stages (wall, summed over parallel requests; phase wall {wall:.2} s):");
        let mut rows: Vec<_> = stages.iter().collect();
        rows.sort_by(|a, b| b.1 .1.total_cmp(&a.1 .1));
        for (name, (n, t)) in rows {
            println!(
                "        {name:22} {n:6} x {:8.1} ms = {t:8.2} s",
                t / *n as f64 * 1000.0
            );
        }
    }
    if !cpu.is_empty() {
        let total: f64 = cpu.iter().map(|c| c.1).sum();
        let groups: Vec<String> = cpu.iter().map(|(g, v)| format!("{g}: {v:.2}")).collect();
        println!(
            "      server CPU {total:.2} s = {:.2} cores busy on average; by thread: {{{}}}",
            if wall > 0.0 { total / wall } else { 0.0 },
            groups.join(", ")
        );
    }

    json!({
        "phase": phase,
        "views": view_times.len(),
        "requests": lat.len(),
        "statuses": statuses,
        "req_p50_ms": ms(pct(&lat, 50.0)),
        "req_p95_ms": ms(pct(&lat, 95.0)),
        "req_max_ms": lat.last().map(|v| ms(*v)),
        "view_mean_s": view_mean,
        "view_max_s": view_max,
        "req_per_s": req_per_s,
        "MB": bytes as f64 / 1e6,
        "cache": cache,
        "wall_s": wall,
        "stages": stages.iter().map(|(k, (n, t))| (k.clone(), json!({"n": n, "total_s": t, "mean_ms": t / *n as f64 * 1000.0}))).collect::<BTreeMap<_, _>>(),
        "cpu_s": cpu.into_iter().collect::<BTreeMap<_, _>>(),
    })
}

fn main() -> Result<()> {
    let args = Args::parse();
    let base = args.url.trim_end_matches('/').to_string();
    let size = match args
        .size
        .split_once('x')
        .map(|(w, h)| (w.parse::<u32>(), h.parse::<u32>()))
    {
        Some((Ok(w), Ok(h))) if w > 0 && h > 0 => (w, h),
        _ => bail!("--size must look like 1920x1080"),
    };
    if args.concurrency == 0 || args.views == 0 {
        bail!("--concurrency and --views must be at least 1");
    }
    let seed = args.seed.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64)
    });
    let agent = agent(Duration::from_secs_f64(args.timeout));
    let points = sample_points(&shapefile_boxes(&args.index)?, args.views, &mut Rng(seed));

    let (views, what): (Vec<Vec<String>>, String) = match &args.mode {
        Mode::Wmts {
            matrix_set,
            level,
            format,
        } => {
            let grid = wmts_grid(&agent, &base, &args.layer, matrix_set, *level, format)?;
            let centres = reproject(&points, &args.index_crs, &grid.crs)?;
            let views = centres
                .iter()
                .map(|&c| {
                    view_tiles(&grid, c, size)
                        .into_iter()
                        .map(|t| tile_url(&grid, matrix_set, *level, t))
                        .collect()
                })
                .collect();
            (
                views,
                format!(
                    "WMTS {matrix_set} level {level} ({:.3} m/px), {}x{} views",
                    grid.res, size.0, size.1
                ),
            )
        }
        Mode::Wms { crs, res, format } => {
            let centres = reproject(&points, &args.index_crs, crs)?;
            let (hw, hh) = (f64::from(size.0) * res / 2.0, f64::from(size.1) * res / 2.0);
            let fmt = format.replace('/', "%2F");
            let views = centres
                .iter()
                .map(|&(cx, cy)| {
                    vec![format!(
                        "{base}/wms?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS={}&STYLES=&CRS={crs}\
                         &BBOX={},{},{},{}&WIDTH={}&HEIGHT={}&FORMAT={fmt}",
                        args.layer,
                        cx - hw,
                        cy - hh,
                        cx + hw,
                        cy + hh,
                        size.0,
                        size.1
                    )]
                })
                .collect();
            (views, format!("WMS {crs} {res} m/px, {}x{} GetMap", size.0, size.1))
        }
    };
    println!(
        "{what}; {} views, {} connections (seed {seed})",
        args.views, args.concurrency
    );

    let phases: &[&str] = if args.no_warm { &["cold"] } else { &["cold", "warm"] };
    let mut results = Vec::new();
    for phase in phases {
        let (before, cpu_before) = (metrics(&agent, &args.metrics), cpu_by_thread(args.pid));
        let (samples, view_times) = run_views(&agent, &views, args.concurrency);
        let (after, cpu_after) = (metrics(&agent, &args.metrics), cpu_by_thread(args.pid));
        results.push(summarize(
            phase,
            &samples,
            &view_times,
            &before,
            &after,
            &cpu_before,
            &cpu_after,
        ));
    }

    if let Some(path) = &args.json {
        let line = json!({"label": args.label, "what": what, "seed": seed, "args": &args, "results": results});
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        writeln!(f, "{line}").map_err(|e| anyhow!("writing {}: {e}", path.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rng_is_reproducible_and_uniform_ish() {
        let (mut a, mut b) = (Rng(7), Rng(7));
        let xs: Vec<f64> = (0..1000).map(|_| a.unit()).collect();
        assert!(xs.iter().zip((0..1000).map(|_| b.unit())).all(|(x, y)| *x == y));
        assert!(xs.iter().all(|x| (0.0..1.0).contains(x)));
        let mean = xs.iter().sum::<f64>() / xs.len() as f64;
        assert!((mean - 0.5).abs() < 0.05, "{mean}");
    }

    #[test]
    fn reads_the_shipped_index() {
        let boxes = shapefile_boxes(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/data/2026_orthoHR/tiles.shp"
        )));
        let Ok(boxes) = boxes else { return };
        assert_eq!(boxes.len(), 498);
        assert!(boxes.iter().all(|b| (b[2] - b[0] - 10_000.0).abs() < 1e-3));
    }

    #[test]
    fn view_covers_the_viewport() {
        let grid = Grid {
            crs: "EPSG:28992".into(),
            res: 1.0,
            origin: (0.0, 1000.0),
            tile: (256, 256),
            matrix: (100, 100),
            template: "x/{TileMatrixSet}/{TileMatrix}/{TileCol}/{TileRow}.jpeg".into(),
        };
        // 512x256 px around (500, 500): x 244..756 -> cols 0..2, y 372..628 -> rows 1..2.
        let tiles = view_tiles(&grid, (500.0, 500.0), (512, 256));
        assert_eq!(tiles, [(0, 1), (1, 1), (2, 1), (0, 2), (1, 2), (2, 2)]);
        assert_eq!(tile_url(&grid, "S", 3, (1, 2)), "x/S/3/1/2.jpeg");
    }

    #[test]
    fn stage_table_reads_deltas() {
        let before: BTreeMap<String, f64> = [
            ("kwms_stage_seconds_sum{stage=\"gdal_run\"}".to_string(), 1.0),
            ("kwms_stage_seconds_count{stage=\"gdal_run\"}".to_string(), 10.0),
        ]
        .into();
        let after: BTreeMap<String, f64> = [
            ("kwms_stage_seconds_sum{stage=\"gdal_run\"}".to_string(), 3.0),
            ("kwms_stage_seconds_count{stage=\"gdal_run\"}".to_string(), 20.0),
            ("kwms_cache_seconds_sum{tier=\"fs\",op=\"read\"}".to_string(), 0.5),
            ("kwms_cache_seconds_count{tier=\"fs\",op=\"read\"}".to_string(), 5.0),
        ]
        .into();
        let t = stage_table(&before, &after);
        assert_eq!(t["gdal_run"], (10, 2.0));
        assert_eq!(t["cache_read[fs]"], (5, 0.5));
    }
}
