# Deployment

Primary artifact: a **container image** containing the kwms binary plus the GDAL runtime libraries it links against. (No `Dockerfile` ships yet; when one is added, give it a `.dockerignore` that excludes `secrets.yaml`, `.secret` and `cache/`.) Works on Kubernetes, Docker Compose, or plain Docker.

Standalone binaries are not a supported distribution channel in v1 (GDAL system-lib version skew is a recurring source of pain). Operators who want bare-metal installs can build from source.

## Behind Istio (or another Envoy sidecar)

kwms speaks plain HTTP/1.1 and expects TLS, retries and rate limiting to be handled in front of it.

- **Port naming.** Name the Service port `http` (or set `appProtocol: http`), not `http2`/`grpc`: the sidecar then talks HTTP/1.1 to kwms, which is all it serves.
- **Metrics.** Leave `server.metrics_bind` at its default `127.0.0.1:9091` and let Istio's *metrics merging* scrape it: annotate the pod with `prometheus.io/scrape: "true"`, `prometheus.io/port: "9091"`, `prometheus.io/path: /metrics`. The sidecar agent fetches it over the pod's loopback and serves it merged with Envoy's own metrics on `:15020/stats/prometheus`, so the metrics port is never exposed.
- **Idle connections.** Envoy keeps a pool of upstream connections; kwms closes a connection that sends nothing for `server.header_read_timeout` (default 10s). Set the upstream idle timeout lower so Envoy closes first and never reuses a connection kwms is closing (which shows up as rare 503s with "upstream connect error … reset before headers"):

  ```yaml
  apiVersion: networking.istio.io/v1
  kind: DestinationRule
  metadata: {name: kwms}
  spec:
    host: kwms
    trafficPolicy:
      connectionPool:
        http:
          idleTimeout: 5s        # < kwms server.header_read_timeout
  ```
- **Probes.** `/healthz` and `/readyz` are on the main port; Istio's probe rewriting works unchanged.
- **Request ids.** Envoy sets `x-request-id`; kwms logs it as `upstream_request_id` next to its own id, so log lines correlate with Istio's access logs.
- **Service links.** The `KWMS_*` variables Kubernetes injects for a Service named `kwms` are ignored (see `configuration.md`); `enableServiceLinks: false` is not needed.
