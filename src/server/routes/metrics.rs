//! `GET /metrics` — Prometheus exposition.
//!
//! The Prometheus recorder is global: we install it once via `OnceLock` on
//! the first metrics request (or on the first explicit `install()` call,
//! whichever happens first) and hand out the rendered exposition string
//! from then on. Installing through `OnceLock` keeps `kwms` test-friendly
//! — re-running tests in the same process doesn't double-install the
//! recorder, which would panic.

use std::sync::OnceLock;

use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

static PROMETHEUS: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the global Prometheus recorder if one isn't installed yet,
/// returning the handle used to render exposition output.
///
/// Idempotent — repeated calls return the same handle.
pub fn install() -> &'static PrometheusHandle {
    PROMETHEUS.get_or_init(|| {
        PrometheusBuilder::new()
            .install_recorder()
            .expect("install prometheus recorder")
    })
}

/// `GET /metrics` — returns the Prometheus exposition format. Always
/// 200 OK; `Content-Type: text/plain; version=0.0.4` per the spec.
pub async fn handler() -> Response {
    let handle = install();
    let body = handle.render();
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
}
