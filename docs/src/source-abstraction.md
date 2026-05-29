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

### Credentials & VSI configuration

GDAL open options are scoped **per source**, not process-wide. `secrets.yaml` declares named credentials blocks; a layer's source references one by name. At dataset open time those become GDAL config options on that specific dataset (e.g. `AZURE_STORAGE_ACCOUNT`, `AZURE_STORAGE_ACCESS_KEY`, `AWS_ACCESS_KEY_ID`, …). This lets one server host layers from multiple accounts/buckets/tenants without bleeding credentials across them.

VSI prefixes supported via GDAL: `vsiaz://`, `vsis3://`, `vsicurl://`, `vsigs://`, plus VRT files referencing any of the above. The source URI is taken verbatim from config — no rewriting.

### Reprojection

When the source's native CRS differs from the requested CRS, reprojection happens inside the GDAL implementation using GDAL's warp API (cheaper than re-warping in pure Rust and gives access to the full GDAL resampling kernel list). Per-layer config picks the resampling method (`nearestneighbour` / `bilinear` / `cubic` / `lanczos` / …); the default is `bilinear`. Token spellings are GDAL's — see `gdal::raster::ResampleAlg::from_str`.
