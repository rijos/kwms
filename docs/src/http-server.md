# HTTP server

## Framework

[`axum`](https://crates.io/crates/axum) on `tokio`. Tower middleware stack carries cross-cutting concerns (tracing spans, metrics, future auth/rate-limit layers). Blocking GDAL work is dispatched via `tokio::task::spawn_blocking` so the async runtime is never blocked.

## Endpoints

- `GET /wms` — WMS 1.1.1 / 1.3.0 (GetCapabilities, GetMap, GetFeatureInfo)
- `GET /wmts` — WMTS 1.0.0 KVP
- `GET /wmts/{layer}/{tilematrixset}/{z}/{x}/{y}.{ext}` — WMTS RESTful
- `GET /tms/1.0.0/{layer}/{z}/{x}/{y}.{ext}` — TMS 1.0.0
- `GET /healthz`, `GET /readyz` — orchestrator probes
- `GET /metrics` — Prometheus exposition

## Auth

Not implemented in v1. The middleware seam is in place, so adding API-key, OIDC, or mTLS authentication later is a localized change. Production deployments are expected to terminate auth at a reverse proxy in front of the server.

## Rate limiting

Out of scope for v1. Delegate to the reverse proxy (nginx, Envoy, Cloudflare, …). Deployment docs call this out so it isn't forgotten.
