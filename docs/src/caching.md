# Caching

## Cache key

Tile-level only: `(layer, z, x, y, format, CRS)`. No separate cache for assembled WMS responses — WMS GetMap is served by looking up the covering tiles in the cache and reassembling. This gives one canonical cache shape across all three protocols.

## Storage stack

Two tiers:

- **Hot tier**: in-process [`moka`](https://crates.io/crates/moka) async cache, bounded by total bytes (configurable). Caches decoded/encoded tile bytes so cache hits skip both source I/O and encoding.
- **Persistent tier**: [`opendal`](https://crates.io/crates/opendal) operator. Backends configurable per deployment — filesystem for single-node, Azure Blob / S3 / Redis / Memcached for multi-instance. OpenDAL's built-in retry and metric layers are enabled.

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

## Invalidation

TTL per layer (and optionally per level rule). Expired entries are treated as misses and re-rendered. No source-mtime checks (too expensive on remote sources), no manual purge endpoint in v1 — operators wanting forced invalidation drop the TTL or clear the backend out-of-band.

## Stampede protection

Single-flight per cache key: concurrent requests for the same missing tile coalesce onto one render. In-process locking is sufficient for v1 (no cross-instance coordination).
