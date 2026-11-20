//! Request-ID middleware.
//!
//! Every request gets an id **we** generate (128 random bits, hex). That
//! id is the only one echoed anywhere: the `X-Request-Id` response header,
//! error bodies, logs. A client-supplied `X-Request-Id` is never echoed; if
//! it is a plain token (`[A-Za-z0-9._-]{1,64}`) it is kept as the
//! *upstream* id and logged next to ours, so proxies can still correlate.
//!
//! Random, not a counter: a sequential id would tell any client how many
//! requests the server handles and when it restarted.

use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::http::{HeaderName, HeaderValue, Request};
use axum::middleware::Next;
use axum::response::Response;

/// Request-id header name. Spelling matches the de-facto-standard
/// `X-Request-Id` used by AWS ALB, GCP HTTP LB, nginx, and most APMs.
pub const HEADER: &str = "x-request-id";

/// The request's ids, stored in request extensions
/// (`Extension<RequestId>` in handlers).
#[derive(Debug, Clone)]
pub struct RequestId {
    id: String,
    upstream: Option<String>,
}

impl RequestId {
    /// Our id: safe to echo anywhere.
    pub fn as_str(&self) -> &str {
        &self.id
    }

    /// The caller's validated `X-Request-Id`, for logs only.
    pub fn upstream(&self) -> Option<&str> {
        self.upstream.as_deref()
    }

    #[cfg(test)]
    pub fn for_tests(id: &str) -> Self {
        Self {
            id: id.to_string(),
            upstream: None,
        }
    }
}

/// A fresh id: 128 bits from the OS random source, as 32 hex digits.
fn fresh_id() -> String {
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        // The OS source doesn't fail on supported platforms; never block a
        // request on it. Unique enough for log correlation.
        static FALLBACK: AtomicU64 = AtomicU64::new(0);
        let n = FALLBACK.fetch_add(1, Ordering::Relaxed);
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        bytes = (t ^ u128::from(n)).to_le_bytes();
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Upstream ids end up in logs, so only plain short tokens are kept.
fn is_acceptable_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// `axum::middleware::from_fn`-compatible handler.
pub async fn middleware(mut req: Request<Body>, next: Next) -> Response {
    let upstream = req
        .headers()
        .get(HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|s| is_acceptable_id(s))
        .map(str::to_string);
    let id = fresh_id();
    req.extensions_mut().insert(RequestId {
        id: id.clone(),
        upstream,
    });

    let mut response = next.run(req).await;
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert(HeaderName::from_static(HEADER), value);
    }
    response
}
