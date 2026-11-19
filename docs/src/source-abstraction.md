# Source abstraction

## Trait shape

A single `Source` trait. Tile math and reprojection live in the server core, not in the trait — the implementation only needs to serve raw windows.

```rust
trait Source: Send + Sync {
    fn metadata(&self) -> SourceMetadata;        // native CRS, native bbox, bands, pixel type, native resolution
    async fn read_window(&self, req: WindowRequest) -> Result<PixelBuffer>;
    async fn sample(&self, coord: Coord, crs: Crs) -> Result<Vec<PixelValue>>;
}
```

`WindowRequest` carries the target bbox, output size in pixels, and target CRS. The pipeline decides what to ask for: when serving a tile, it computes the bbox in the tile's grid CRS and asks the source for a window at the tile's pixel size. GDAL's own warping handles the reprojection inside the implementation.

The trait exists as an **internal seam** between raster access and the cache/pipeline/protocol code that consumes it — not as a hedge for swappable backends. There's one implementation (`source::gdal::GdalSource`) and we don't plan to add more. The trait pays for itself by making the pipeline easy to unit-test with a mock source.

## GDAL implementation

### Dataset pooling

Each configured source keeps a bounded pool of open GDAL datasets. Requests check out a handle, use it, return it. Pool size is configurable per source (defaults to small — GDAL dataset handles can be heavy, especially for VRT mosaics and remote VSI sources).

GDAL operations are blocking; pool checkouts and reads run on `tokio::task::spawn_blocking` (or a dedicated blocking thread pool). The `gdal` crate's `Dataset` is not `Send`-safe across arbitrary threads in all configurations — the pool holds handles in slots that are only ever touched while owned by the caller, never shared mid-flight.

### Overviews or bail (source-read budget)

Every tile reads its source through `GDALWarp` with `-ovr AUTO`: GDAL reads the coarsest overview whose decimation suits the tile, or the full-resolution raster when none does. Without overviews a coarse tile therefore reads the whole source: one z0 tile from a 40000×40000 GeoTIFF took 12 s and 7 GB of RAM (0.08 s with overviews). Tile coordinates are client-chosen, so this would let any client stall a source.

At startup, for every layer and every level of every grid it is served in, kwms estimates what the worst tile reads — mirroring GDAL's overview choice (`pipeline/read_budget.rs`) — and **refuses to start** if any tile would read more than 4096² source pixels per band:

```text
source `ortho.tif` lacks overviews: a tile of grid `webmercator` at level 0 would read
about 1600000000 source pixels (limit 16777216); it needs an overview decimated at
least 10x (e.g. `gdaladdo -r average <file> 2 4 8 16 …`)
```

Small sources (≤ 4096² pixels) need no overviews. Large ones need a pyramid down to about tile size: `gdaladdo -r average <file> 2 4 8 16 32 64 128 256` (or COG/VRT with overviews). Measured with such a pyramid, the slowest tile at any level of the PDOK grid set was 45 ms.

### Credentials & VSI configuration

GDAL credentials are scoped **per object-store path prefix**, not process-wide. `secrets.yaml` declares named credentials blocks; a layer's source references one by name and, optionally, the prefix it applies to (`credentials_prefix`, default: the `/vsiXXX/<bucket>/` of the source URI). At startup, before any dataset is opened, kwms registers each block with `VSISetPathSpecificOption` (`source::gdal::credentials`). This lets one server host layers from multiple accounts/buckets/tenants without bleeding credentials across them.

Two simpler mechanisms don't work, which is why it is done this way:

- **Dataset open options.** VSI handlers never read credentials from open options; drivers just warn about unknown options.
- **Thread-local config options** on the dataset-pool worker. Mosaic drivers (GTI, VRT) read tiles on GDAL's own thread pool, which doesn't inherit them, so remote reads fail with "No valid Azure credentials". Forcing those drivers to one thread makes it work, but a country-wide GTI read then took 68 s instead of 4 s.

Path-specific options are process-wide state that GDAL reads without holding its lock, so kwms writes each (prefix, option) at most once and never changes it while serving (see [Unsafe code audit](./unsafe-audit.md), F18).

VSI prefixes supported via GDAL: `/vsiaz/`, `/vsis3/`, `/vsicurl/`, `/vsigs/`, plus VRT files and GTI tile indexes referencing any of them. The source URI is taken verbatim from config — no rewriting — and `open_options` are passed as GDAL open options (`KEY=VALUE`).

### Tile indexes (GTI)

A mosaic of many COGs can be served from a vector tile index (shapefile, GeoPackage, FlatGeobuf with a location field) through GDAL's GTI driver: `uri: "GTI:data/2026_orthoHR/tiles.shp"`. Nothing about the tiles needs configuring. At startup kwms reads every tile's header, 32 in parallel (`source::gdal::tile_index`). For the 498 remote COGs of the 2026 ortho that takes about 2.5 s.

- **GSD per area.** An index doesn't record the tiles' resolution, and GTI on its own takes it from the *first* tile, squeezing every other tile onto that grid with nearest neighbour. On the 2026 ortho (148 tiles at 5 cm, 350 at 8 cm) that made the 5 cm areas blocky. kwms instead groups the tiles by GSD and opens one GTI view per GSD. Each read uses the view of the finest tiles under the requested window, so an 8 cm area is read at 8 cm and a 5 cm area at 5 cm. A window spanning both is read on the finer grid, with the coarser tiles resampled bilinearly (`RESAMPLING=bilinear`, not GTI's nearest). Tile footprints are reprojected once per request CRS, on the blocking pool.
- **Overviews.** A mosaic has none, so a coarse tile would read the full raster and the read budget above would refuse it. kwms adds virtual ones per view (`OVERVIEW_<n>_FACTOR` = 2, 4, …, until the whole mosaic fits 4096 px). GTI serves them from each tile's own overviews.
- **Reporting.** The startup log reports tiles per GSD, tiles that couldn't be opened and tiles without overviews.

Options set in `open_options`, or stored in the index (e.g. layer metadata of a `.gti.gpkg`), are never overridden. Setting `RESX`/`RESY` pins one resolution for the whole mosaic.

Coarse levels touch many tiles: one tile covering the whole country opens all of them, which took about 2.3 s cold for the 498-COG 2026 ortho mosaic. A coarse WMS GetMap is composed from such grid tiles, so a cold country-wide GetMap renders dozens of them and can exceed `request_timeout`. Seed the coarse levels with `kwms cache update` (see [Caching](./caching.md)). Finer GetMaps are read directly and don't depend on the cache.

### Reprojection

When the source's native CRS differs from the requested CRS, reprojection happens inside the GDAL implementation using GDAL's warp API (cheaper than re-warping in pure Rust and gives access to the full GDAL resampling kernel list). Per-layer config picks the resampling method (`nearestneighbour` / `bilinear` / `cubic` / `lanczos` / …); the default is `bilinear`. Token spellings are GDAL's — see `gdal::raster::ResampleAlg::from_str`.
