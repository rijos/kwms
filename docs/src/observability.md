# Observability

- **Structured logs** via the [`tracing`](https://crates.io/crates/tracing) crate, JSON output. Each request gets a span carrying request id, protocol, layer, tile coords (when applicable), cache outcome (hit/miss/passthrough), source latency, and response status.
- **Prometheus metrics** at `/metrics`: request counts and latencies (by protocol/layer/status), cache hit/miss/passthrough counters, hot-tier size and eviction counts, source pool wait time and checkout count, single-flight coalescing counts, error counts by class.
- **Health & readiness** endpoints: `/healthz` always returns 200 once the process is up; `/readyz` returns 200 only once config is parsed, source pools are warmed (at least one dataset open per source), and the cache backend is reachable.
- **No OpenTelemetry tracing in v1** — the tracing crate's structured spans are enough; OTLP export can be added later via `tracing-opentelemetry` without touching application code.
