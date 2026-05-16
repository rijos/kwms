//! Request-ID middleware.
//!
//! Reads an inbound `X-Request-Id` header (if present) or generates a new
//! one as a monotonic hex counter, stores it in request extensions so
//! handlers can read it via `Extension<RequestId>`, and echoes it back on
//! the response.
//!
//! We deliberately avoid pulling in `uuid` / `rand` — a process-local
//! `AtomicU64` is plenty for log correlation and traces never leave the
//! origin host as their canonical id.

use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Request};
use axum::middleware::Next;
use axum::response::Response;

/// Request-id header name. Spelling matches the de-facto-standard
/// `X-Request-Id` used by AWS ALB, GCP HTTP LB, nginx, and most APMs.
pub const HEADER: &str = "x-request-id";

/// Newtype wrapper for a request id. Stored in axum request/response
/// extensions; handlers wanting to log it should pull it out via
/// `request.extensions().get::<RequestId>()`.
#[derive(Debug, Clone)]
pub struct RequestId(pub String);

impl RequestId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a fresh id. Hex-encoded `AtomicU64` — short, monotonic, and
/// uniquely identifies a request within a single process lifetime.
fn fresh_id() -> String {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{n:016x}")
}

/// `axum::middleware::from_fn`-compatible handler.
///
/// Pulls `X-Request-Id` from the request (if a client supplied one), or
/// generates one, then:
///   * injects a `RequestId` extension on the request before the inner
///     service sees it, so handlers and tracing spans can pick it up;
///   * sets the same id on the outbound `X-Request-Id` response header so
///     callers can correlate logs across services.
pub async fn middleware(mut req: Request<Body>, next: Next) -> Response {
    let id = req
        .headers()
        .get(HEADER)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string())
        .unwrap_or_else(fresh_id);

    req.extensions_mut().insert(RequestId(id.clone()));

    let mut response = next.run(req).await;

    if let Ok(value) = HeaderValue::from_str(&id) {
        response
            .headers_mut()
            .insert(HeaderName::from_static(HEADER), value);
    }
    response
}
