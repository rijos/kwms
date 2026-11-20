# Caching

## Cache key

Tile-level only: `(layer, grid, z, x, y, format)`. No separate cache for assembled WMS responses. One canonical cache shape serves all three protocols: a tile cached for a WMTS request also serves TMS and WMS, and the other way round.

### WMS: tiles within the cached pyramid, direct reads beyond it

A GetMap is served one of two ways, chosen per request (`pipeline::wms_assemble`):

- **Within the cached pyramid**: the request is at least as coarse as the deepest durably cached level (`cache.max_level`, below). It is composed from grid tiles: the zoom level is picked from the requested resolution, the covering tiles come through the cache (where `kwms cache update` put them), and one GDAL warp mosaics them to the requested bbox and size.
- **Beyond it**: the request is finer than that level. kwms reads the requested window straight from the source in one GDAL warp at exactly the requested size, and caches nothing. Tiles there aren't stored durably anyway, so composing them would mean many separate source reads plus a second resampling. Rounding to a tile level would also upsample tiles up to 2x coarser than requested.

On the 2026 ortho (1920x1080 GetMap, cold, release build), direct reads took 0.40 s at 0.08 m/px vs 1.9 s from tiles, and 0.94 s vs 1.6 s at 0.25 m/px. The images are also visibly sharper. An identical repeated request costs 0.25–0.5 s, since nothing is cached; a panning client rarely repeats a bbox exactly.

A direct read must stay within `read_budget::MAX_DIRECT_READ_PIXELS` (four tiles' worth of source pixels). Request sizes are client-chosen, so on a source with sparse overviews a larger read is composed from tiles instead, each bounded by the per-tile budget.

## Storage stack: tiers

The cache is an ordered stack of **tiers** (`cache.tiers` in `config.yaml`, see [Configuration](./configuration.md)), all behind one interface, `cache::CacheTier`:

| | |
|---|---|
| `get(key, ttl)` | the tile and its render time, if present and younger than `ttl` |
| `put(key, entry)` | store a tile, keeping its render time |
| `check()` | startup check (stores write, read back and delete a probe) |
| `name()`, `durable()` | label for logs and metrics; whether it survives a restart and is shared |

Two kinds ship:

- **`memory`**: in-process [`moka`](https://crates.io/crates/moka) cache bounded by total bytes. Not durable.
- **`store`**: any [`opendal`](https://crates.io/crates/opendal) backend. `backend:` is an opendal service name and `options:` goes verbatim to [`Operator::via_iter`](https://docs.rs/opendal/0.59/opendal/struct.Operator.html#method.via_iter) (keys per backend: <https://docs.rs/opendal/0.59/opendal/services/>). Compiled in: `fs` (local disk), `azblob` (Azure Blob Storage) and `memory`; others need their opendal `services-*` feature in `Cargo.toml`.

`cache::LayeredCache` composes them:

- **Lookup** goes top to bottom. A hit in a lower tier is copied into every tier above it with its original render time, so the TTL counts from the render in every tier: copying never makes a tile younger.
- **A rendered tile** is written to every tier.
- **Single-flight** (below) wraps the whole stack.

The default is `memory` (512 MB) over `store`/`fs` (`/var/cache/kwms`). A typical production stack is memory → local disk → shared Azure container: each server keeps its hot set locally, and every server (and `kwms cache update`) shares the Azure tier. Adding a new kind of tier means implementing `CacheTier` and adding a `TierConfig` variant.

### Layout: one prefix per layer

Durable tiers store each tile as one object, plus a small `.meta` JSON sidecar holding its render time (Unix seconds, UTC; the TTL check) and its render fingerprint (see *Invalidation*):

```text
<root>/<layer>/<epsg>/<format>/<z>/<x>/<y>.<ext>
<root>/2026_orthoHR/28992/jpeg/16/31417/33299.jpeg
<root>/2026_orthoHR/28992/jpeg/16/31417/33299.jpeg.meta
```

- **One prefix per layer.** Layers never share or overwrite tiles, even when they read the same source. One layer can be inspected, sized or cleared on its own (delete `<layer>/`).
- **Grid directory.** The second segment is the grid's EPSG code, or the grid's config name for a CRS without an EPSG code. A layer may therefore have only one grid per CRS; config validation rejects two.
- **Names.** Layer and custom-grid names must be plain path segments (`A-Z a-z 0-9 _ - .`, not starting with `.`); config validation enforces it.
- **Stability.** Paths contain no hashes, so they stay the same across kwms builds and Rust versions.

Renaming a layer starts it with an empty cache. Changing a layer's render settings (sources, mask, grids, encoder settings) keeps the paths; the next `kwms cache update` replaces the affected tiles (see *Invalidation*).

- **Atomic writes.** A tile is complete or absent, never partly written. Azure Blob writes are atomic per object. The `fs` backend writes each object to a temporary file in `<root>/.kwms-tmp/` and renames it into place (opendal's `atomic_write_dir`, set automatically; same file system, so the rename is atomic). The data is written before its sidecar, so a reader never sees a sidecar without its data.
- **Timestamps from the future** (past now plus 60 s of clock skew) are not trusted: such a tile counts as expired. Otherwise it would never expire, and `cache update` would never replace it.

### Tier health

- **At startup** kwms runs every tier's check. A store writes, reads back and deletes a probe object (`.kwms-startup-check`, a name no layer can have) and refuses to start if any step fails. A missing container, wrong endpoint, bad key or a token without write/delete permission is therefore a startup error, not a cache that silently never fills.
- **While serving**, a tier error (outage, throttling) does not fail the request. A read error counts as a miss in that tier and the lookup continues below it, ending at the source. A write error leaves the tile in the other tiers. Each error is logged (`cache tier error; serving without it`) and counted in `kwms_cache_errors_total{tier, op="read"|"write"}`. Alert on that counter: a sustained rate means requests are falling through to the source.
- **Hits and misses** are counted in `kwms_cache_hits_total{tier}` and `kwms_cache_misses_total`.

## Filling the cache ahead of requests

`kwms cache update` renders tiles into the **durable** tiers, using the same key, rendering and "no empty tiles" rule as serving (usage and flags: [Configuration](./configuration.md#filling-the-cache-kwms-cache-update)). Its default range, `--max-level auto`, is every level whose tiles are larger than one source file:

- **A tile larger than a file** opens several source files. A country-wide tile of the 2026 ortho opens all 498 remote COGs, which takes seconds. These are the tiles worth caching.
- **A tile smaller than a file** opens one to four, each through the file's own overviews: about as cheap as reading a cached tile, so caching buys little.

The file size is the median tile extent of a GTI index. Any other source counts as one file, so only the levels where one tile covers the whole source are filled. Pass `--max-level` for VRT mosaics.

Unlike serving, a seeding run surfaces cache errors instead of degrading: a tile counts as stored only once every durable tier has it. A fresh copy found in one durable tier is copied to the others instead of being re-rendered.

## Cache policy

Per-layer, with optional per-level overrides. Each layer config declares a default mode and may add level-range rules:

```yaml
cache:
  mode: cache          # cache | memory | passthrough
  max_level: auto      # deepest level stored durably: auto (default) | <level> | all
  ttl: 7d
  level_rules:
    - levels: 15-22
      mode: passthrough  # deep levels bypass cache; rendered fresh each request
```

Three modes:

- **`cache`** uses every tier.
- **`memory`** uses only the in-process tiers: recently served tiles are kept in RAM, but nothing is written to disk or object storage.
- **`passthrough`** reads and writes no tier: rendered from source on every request.

No DSL beyond level ranges in v1.

### Only coarse levels are stored durably

`max_level` caps how deep a layer is stored in the durable tiers. Below it, `cache` levels behave as `memory`.

The default, `auto`, is per grid: the deepest level whose tiles are larger than one source file (`pipeline::cache_levels`). Coarser tiles open many source files and are slow to render. Finer tiles open one to four files through their own overviews and cost about the same as reading the source GeoTIFF directly, so storing them buys little. For the 2026 ortho (10 km COGs) that is RD level 6 and Web Mercator level 11.

Measured mean GDAL read per cold RD tile, release build, Azure source:

| level | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 |
|---|---|---|---|---|---|---|---|---|
| ms | 1469 | 655 | 265 | 92 | 73 | 60 | 54 | 51 |

`kwms cache update` fills exactly these levels. An explicit `--max-level` deeper than `max_level` is capped, because the server would never read those tiles from storage. The limits are logged at startup (`durable cache levels`).

**Empty tiles are never cached.** Clients choose tile coordinates, so nothing they request may fill the cache with tiles that hold no data:

- a tile outside the source's coverage is served as a fill tile without reading the source or touching the cache;
- a tile inside the coverage that comes back entirely nodata (or entirely 0 when the source has no nodata) is served but not stored. The coverage is the source extent reprojected to the grid's CRS, a superset of the data that can be far larger than it (an Antarctic source covers 7×10⁸ m around RD New), so without this the guard above would only hold for same-CRS sources. A genuinely black tile without nodata is also re-rendered each time; that only costs a render, which the source-read budget bounds (`source-abstraction.md`).

## Measuring where time goes

Every request stage is timed in `kwms_stage_seconds{stage}`:

| stage | what it measures |
|---|---|
| `gdal_wait` | waiting for a free dataset |
| `gdal_run` | the GDAL read and warp, including network I/O |
| `cpu_wait` | waiting for a render slot |
| `encode` | encoding the tile |
| `wms_tiles` | WMS: getting the covering tiles |
| `wms_direct` | WMS beyond the cached pyramid: direct read and encode |
| `wms_composite` | WMS: mosaicking and encoding the image |

Each cache tier operation is timed in `kwms_cache_seconds{tier, op}`. Every metric is a summary, so `_sum` and `_count` give totals.

`kwms-bench` (`src/bench/kwms-bench.rs`; `cargo run --release --features bindgen,bench --bin kwms-bench -- --help`) loads QGIS-like views (all tiles of a 1920×1080 viewport over 6 connections, or one WMS GetMap), first cold and then warm. Per phase it prints:

- latency percentiles;
- these stage totals;
- with `--pid`, the server's CPU time per thread name.

GDAL's own worker threads inherit the `gdal-pool` name, so that group covers all GDAL work. Comparing `gdal_run` wall time with `gdal-pool` CPU time shows how much of a read is spent waiting on the network.

## Invalidation

TTL per layer (and optionally per level rule). Expired entries are treated as misses and re-rendered. No source-mtime checks (too expensive on remote sources), no manual purge endpoint in v1 — operators wanting forced invalidation drop the TTL or clear the backend out-of-band.

**Settings changes are applied by a cache build, not while serving** (a deliberate trade-off). Every stored tile records a fingerprint of what decided its pixels (`pipeline::fingerprint`): the layer's sources and open options, resampling, nodata, grids (with custom grid definitions), effective encoder settings, the *contents* of its `source_mask` file, and a render revision kwms bumps when it renders the same settings differently. `kwms cache update` treats a tile with another fingerprint (or none) as stale and renders it again. Serving ignores the fingerprint: after changing, say, the mask, the old tiles stay in use until the next update or their TTL, so a deploy never triggers a re-render storm. Titles, abstracts, cache and `Cache-Control` settings are not part of the fingerprint.

## Stampede protection

Single-flight per cache key: concurrent requests for the same missing tile coalesce onto one render. Requests that were waiting get the render's result directly, so an empty tile (not cached, above) is still rendered once per burst, not once per waiter. In-process locking is sufficient for v1 (no cross-instance coordination). See `concurrency.md`.
