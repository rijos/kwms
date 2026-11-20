# HTTP server

## Framework

[`axum`](https://crates.io/crates/axum) on `tokio`, served through hyper's **HTTP/1.1** connection builder (`subcommand::serve::serve_http1`). kwms runs behind a TLS-terminating proxy (an ingress, Istio's sidecar); it never speaks HTTP/2 or cleartext h2c. Tower middleware stack carries cross-cutting concerns (tracing spans, metrics, future auth/rate-limit layers). Blocking GDAL work never runs on the async workers: source reads go through each source's dataset pool, and CPU-bound work (encoding, WMS compositing) runs via `tokio::task::spawn_blocking` behind a semaphore of `server.render_concurrency` slots (default: CPU count). A job that can't get a slot within `server.render_queue_timeout` (default 5s) gets 429 Too Many Requests with `Retry-After` — a capacity signal, not a server failure.

## Endpoints

- `GET /wms` — WMS 1.1.1 / 1.3.0 (GetCapabilities, GetMap, GetFeatureInfo)
- `GET /wmts` — WMTS 1.0.0 KVP
- `GET /wmts/{Layer}/{TileMatrixSet}/{TileMatrix}/{TileCol}/{TileRow}.{ext}` — WMTS RESTful (the advertised `ResourceURL` template; same shape as PDOK)
- `GET /wmts/1.0.0/WMTSCapabilities.xml` — WMTS RESTful GetCapabilities
- `GET /tms/1.0.0/{layer}/{z}/{x}/{y}.{ext}` — TMS 1.0.0
- `GET /healthz`, `GET /readyz` — orchestrator probes
- `GET /metrics` — Prometheus exposition, on the separate `server.metrics_bind` listener (not the public port)

## Connections

- **Header timeout** (`server.header_read_timeout`, default 10s): a client gets this long to send a request's headers, and an idle keep-alive connection is closed after about as long. Slow-header ("slowloris") clients can't hold connections open. (With `axum::serve` this timeout was silently off: hyper needs a timer to enforce it.)
- **Connection cap** (`server.max_connections`, default 4096): at most this many open connections; further ones wait in the kernel's listen backlog, costing nothing until a slot frees up. The metrics listener allows 64.
- Behind a proxy that pools upstream connections (Envoy/Istio), set the proxy's idle timeout *below* `header_read_timeout`, so the proxy closes idle connections first and never reuses one kwms is closing (see `deployment.md`).

## Response headers

- **`X-Request-Id`**: 128 random bits per request (32 hex digits) — not a counter, which would reveal request volume and restarts.
- **`Cache-Control`** comes from the config: `server.cache_control.images` for tiles and GetMap images (a layer's `cache_control` replaces it), `server.cache_control.capabilities` for capabilities documents. Every other response — errors, WMS exceptions (HTTP 200, so a cache can't tell by status), overload and panic replies, health checks — gets `no-store`, so a CDN never keeps a transient failure. An empty value sends no header.

## Request limits

- **WMS GetMap**: `WIDTH`/`HEIGHT` above `server.max_request_pixels` are rejected; `BBOX` values must be finite; the covering tile set is clamped to the grid and capped.
- **CRS parameters** from clients must be `EPSG:<code>` or `CRS:84`/`CRS:83`/`CRS:27`. Anything else is rejected before reaching GDAL, whose `OSRSetFromUserInput` would otherwise load file paths and fetch URLs.
- **Tiles** (WMTS/TMS/WMS) must lie inside the grid's advertised levels and tile matrix, in a format listed in the layer's `formats:`. Tiles and GetMap boxes with no source file footprint (or no `source_mask` area) under them are served as fill images without source reads; fill images are never stored in the tile cache, and their encoded bytes are kept in memory per layer, format and size.
- **WMS reads past the cached pyramid** go straight to the source only if the box overlaps at most 4 source files (and the read stays within the pixel budget); otherwise the image is composed from tiles, each of which reads at most a few files and stays in memory for the next request.
- **Overload**: WMS/WMTS/TMS routes share a limit of `server.max_concurrent_requests` in-flight requests (excess shed immediately with 429 Too Many Requests, a capacity signal) and a `server.request_timeout` budget (503 when exceeded, since that can mean a stuck backend). `/healthz` and `/readyz` are exempt, so an overloaded server is not restarted by its orchestrator.

## Auth

Not implemented in v1. The middleware seam is in place, so adding API-key, OIDC, or mTLS authentication later is a localized change. Production deployments are expected to terminate auth at a reverse proxy in front of the server.

## Rate limiting

Out of scope for v1. Delegate to the reverse proxy (nginx, Envoy, Cloudflare, …). Deployment docs call this out so it isn't forgotten.
