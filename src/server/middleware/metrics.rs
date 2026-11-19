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

/// Method label from a fixed set: clients choose the method token freely,
/// and every distinct label value is a new Prometheus series kept forever.
fn method_label(method: &axum::http::Method) -> &'static str {
    use axum::http::Method;
    match *method {
        Method::GET => "GET",
        Method::HEAD => "HEAD",
        Method::OPTIONS => "OPTIONS",
        Method::POST => "POST",
        _ => "other",
    }
}

/// `axum::middleware::from_fn`-compatible handler. Wraps every request
/// in a stopwatch and emits a counter + histogram on completion.
pub async fn middleware(req: Request<Body>, next: Next) -> Response {
    let method = method_label(req.method());
    let path = req.uri().path().to_string();
    let route = route_label(&path);
    // Access log, off unless `RUST_LOG` enables debug for this module (e.g.
    // `RUST_LOG=info,kwms::server::middleware::metrics=debug`): what
    // clients actually ask for (QGIS image sizes, CRS, formats). The URI is
    // capped so a hostile query string can't bloat the log.
    let access =
        tracing::enabled!(tracing::Level::DEBUG).then(|| req.uri().to_string().chars().take(2048).collect::<String>());

    let started = Instant::now();
    let response = next.run(req).await;
    let elapsed = started.elapsed().as_secs_f64();

    let status: StatusCode = response.status();
    let status_label = status.as_u16().to_string();

    metrics::counter!(
        "kwms_http_requests_total",
        "method" => method,
        "path" => route,
        "status" => status_label.clone(),
    )
    .increment(1);

    metrics::histogram!(
        "kwms_http_request_duration_seconds",
        "method" => method,
        "path" => route,
        "status" => status_label,
    )
    .record(elapsed);

    if let Some(uri) = access {
        tracing::debug!(method, uri = %uri, status = status.as_u16(), ms = elapsed * 1000.0, "request");
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_label_is_bounded() {
        use axum::http::Method;
        assert_eq!(method_label(&Method::GET), "GET");
        assert_eq!(method_label(&Method::from_bytes(b"AAAAAAAAAAAAA1").unwrap()), "other");
        assert_eq!(method_label(&Method::DELETE), "other");
    }

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
