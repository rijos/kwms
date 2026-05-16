//! Health / readiness probe endpoints.
//!
//! * `/healthz` — liveness. Always 200 once the process is up. No
//!   dependencies are checked.
//! * `/readyz` — readiness. 200 only when the app is ready to serve real
//!   traffic, defined here as "at least one source dataset has been opened
//!   successfully" (i.e. `AppState::new` got past `GdalSource::open`). The
//!   cache backend's reachability is implicitly proven by `AppState::new`
//!   succeeding too — `LayeredCache::new` returns an error if the
//!   persistent operator can't be built.
//!
//! Kept deliberately tiny — these probes are hit dozens of times per
//! second by orchestrators and shouldn't add allocations or locks.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::server::AppState;

/// `GET /healthz` — always returns 200 OK with the body `"ok"`.
pub async fn healthz() -> &'static str {
    "ok"
}

/// `GET /readyz` — 200 if at least one source is registered, 503 otherwise.
pub async fn readyz(State(app): State<Arc<AppState>>) -> Response {
    if app.source_count > 0 {
        (StatusCode::OK, "ready").into_response()
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, "no sources registered").into_response()
    }
}
