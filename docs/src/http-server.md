# HTTP server

## Framework

[`axum`](https://crates.io/crates/axum) on `tokio`. Tower middleware stack carries cross-cutting concerns (tracing spans, metrics, future auth/rate-limit layers). Blocking GDAL work never runs on the async workers: source reads go through each source's dataset pool, and CPU-bound work (encoding, WMS compositing) runs via `tokio::task::spawn_blocking` behind a semaphore of `server.render_concurrency` slots (default: CPU count). A job that can't get a slot within `server.render_queue_timeout` (default 5s) gets 429 Too Many Requests with `Retry-After` — a capacity signal, not a server failure.

## Endpoints

- `GET /wms` — WMS 1.1.1 / 1.3.0 (GetCapabilities, GetMap, GetFeatureInfo)
- `GET /wmts` — WMTS 1.0.0 KVP
- `GET /wmts/{Layer}/{TileMatrixSet}/{TileMatrix}/{TileCol}/{TileRow}.{ext}` — WMTS RESTful (the advertised `ResourceURL` template; same shape as PDOK)
- `GET /wmts/1.0.0/WMTSCapabilities.xml` — WMTS RESTful GetCapabilities
- `GET /tms/1.0.0/{layer}/{z}/{x}/{y}.{ext}` — TMS 1.0.0
- `GET /healthz`, `GET /readyz` — orchestrator probes
- `GET /metrics` — Prometheus exposition, on the separate `server.metrics_bind` listener (not the public port)

## Request limits

- **WMS GetMap**: `WIDTH`/`HEIGHT` above `server.max_request_pixels` are rejected; `BBOX` values must be finite; the covering tile set is clamped to the grid and capped.
- **CRS parameters** from clients must be `EPSG:<code>` or `CRS:84`/`CRS:83`/`CRS:27`. Anything else is rejected before reaching GDAL, whose `OSRSetFromUserInput` would otherwise load file paths and fetch URLs.
- **Tiles** (WMTS/TMS/WMS) must lie inside the grid's advertised levels and tile matrix, in a format listed in the layer's `formats:`. Tiles entirely outside the source's coverage are served as fill tiles without source reads and are not cached.
- **Overload**: WMS/WMTS/TMS routes share a limit of `server.max_concurrent_requests` in-flight requests (excess shed immediately with 429 Too Many Requests, a capacity signal) and a `server.request_timeout` budget (503 when exceeded, since that can mean a stuck backend). `/healthz` and `/readyz` are exempt, so an overloaded server is not restarted by its orchestrator.

## Auth

Not implemented in v1. The middleware seam is in place, so adding API-key, OIDC, or mTLS authentication later is a localized change. Production deployments are expected to terminate auth at a reverse proxy in front of the server.

## Rate limiting

Out of scope for v1. Delegate to the reverse proxy (nginx, Envoy, Cloudflare, …). Deployment docs call this out so it isn't forgotten.
