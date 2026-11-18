# Caching

## Cache key

Tile-level only: `(layer, z, x, y, format, CRS)`. No separate cache for assembled WMS responses — WMS GetMap is served by looking up the covering tiles in the cache and reassembling. This gives one canonical cache shape across all three protocols.

## Storage stack

Two tiers:

- **Hot tier**: in-process [`moka`](https://crates.io/crates/moka) async cache, bounded by total bytes (configurable). Caches decoded/encoded tile bytes so cache hits skip both source I/O and encoding.
- **Persistent tier**: [`opendal`](https://crates.io/crates/opendal) operator. We don't enumerate backends or per-backend options ourselves — the `backend:` key in `config.yaml` is any [opendal `Scheme`](https://docs.rs/opendal/0.50/opendal/enum.Scheme.html) name (`fs`, `s3`, `azblob`, `gcs`, `redis`, `memcached`, `webdav`, `hdfs`, …) and the `options:` map is passed verbatim to [`Operator::via_iter`](https://docs.rs/opendal/0.50/opendal/struct.Operator.html#method.via_iter). Per-backend option keys are documented at <https://docs.rs/opendal/0.50/opendal/services/>. OpenDAL's built-in retry and metric layers are enabled.

Lookup order: hot → persistent → source (render + populate both tiers on miss).

## Cache policy

Per-layer, with optional per-level overrides. Each layer config declares a default mode and may add level-range rules:

```yaml
cache:
  mode: cache          # cache | passthrough
  ttl: 7d
  level_rules:
    - levels: 15-22
      mode: passthrough  # deep levels bypass cache; rendered fresh each request
```

`cache` writes to both tiers on miss. `passthrough` reads/writes neither — rendered from source every request. No DSL beyond level ranges in v1.

**Empty tiles are never cached.** Clients choose tile coordinates, so nothing they request may fill the cache with tiles that hold no data:

- a tile outside the source's coverage is served as a fill tile without reading the source or touching the cache;
- a tile inside the coverage that comes back entirely nodata (or entirely 0 when the source has no nodata) is served but not stored. The coverage is the source extent reprojected to the grid's CRS, a superset of the data that can be far larger than it (an Antarctic source covers 7×10⁸ m around RD New), so without this the guard above would only hold for same-CRS sources. A genuinely black tile without nodata is also re-rendered each time; that only costs a render, which the source-read budget bounds (`source-abstraction.md`).

## Invalidation

TTL per layer (and optionally per level rule). Expired entries are treated as misses and re-rendered. No source-mtime checks (too expensive on remote sources), no manual purge endpoint in v1 — operators wanting forced invalidation drop the TTL or clear the backend out-of-band.

## Stampede protection

Single-flight per cache key: concurrent requests for the same missing tile coalesce onto one render. Requests that were waiting get the render's result directly, so an empty tile (not cached, above) is still rendered once per burst, not once per waiter. In-process locking is sufficient for v1 (no cross-instance coordination). See `concurrency.md`.
