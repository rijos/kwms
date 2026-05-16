//! Per-request metrics middleware.
//!
//! Records:
//! - `kwms_http_requests_total{method,path,status}` (counter)
//! - `kwms_http_request_duration_seconds{method,path,status}` (histogram)
//!
//! Labels intentionally use the *route template* segment ("/wms", "/wmts",
//! "/tms", "/healthz", "/readyz", "/metrics", or the first path segment for
//! anything else) rather than the raw URI — recording every tile URL as a
//! distinct label would explode metric cardinality.

use std::time::Instant;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

/// Reduce a request path to a low-cardinality label.
///
/// We bucket everything under its first non-empty path segment:
///   * `/wms?...` → `/wms`
///   * `/wmts/foo/bar/0/0/0.png` → `/wmts`
///   * `/tms/1.0.0/foo/0/0/0.png` → `/tms`
///   * `/healthz` → `/healthz`
///   * `/` → `/`
fn route_label(path: &str) -> &'static str {
    let trimmed = path.trim_start_matches('/');
    let first = trimmed.split('/').next().unwrap_or("");
    match first {
        "wms" => "/wms",
        "wmts" => "/wmts",
        "tms" => "/tms",
        "healthz" => "/healthz",
        "readyz" => "/readyz",
        "metrics" => "/metrics",
        "" => "/",
        _ => "/other",
    }
}

/// `axum::middleware::from_fn`-compatible handler. Wraps every request
/// in a stopwatch and emits a counter + histogram on completion.
pub async fn middleware(req: Request<Body>, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let route = route_label(&path);

    let started = Instant::now();
    let response = next.run(req).await;
    let elapsed = started.elapsed().as_secs_f64();

    let status: StatusCode = response.status();
    let status_label = status.as_u16().to_string();
    let method_label = method.as_str().to_string();

    metrics::counter!(
        "kwms_http_requests_total",
        "method" => method_label.clone(),
        "path" => route,
        "status" => status_label.clone(),
    )
    .increment(1);

    metrics::histogram!(
        "kwms_http_request_duration_seconds",
        "method" => method_label,
        "path" => route,
        "status" => status_label,
    )
    .record(elapsed);

    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_label_buckets_protocols() {
        assert_eq!(route_label("/wms"), "/wms");
        assert_eq!(route_label("/wmts/a/b/c"), "/wmts");
        assert_eq!(route_label("/tms/1.0.0/x/0/0/0.png"), "/tms");
        assert_eq!(route_label("/healthz"), "/healthz");
        assert_eq!(route_label("/readyz"), "/readyz");
        assert_eq!(route_label("/metrics"), "/metrics");
        assert_eq!(route_label("/"), "/");
        assert_eq!(route_label("/something-else"), "/other");
    }
}
