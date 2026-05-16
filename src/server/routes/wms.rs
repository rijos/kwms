//! `GET /wms` — WMS 1.1.1 / 1.3.0 handler.
//!
//! Dispatch is straightforward: parse the raw query string, branch on the
//! parsed [`WmsRequest`], delegate to the protocol/pipeline modules, then
//! shape an axum [`Response`] with the right `Content-Type`.
//!
//! Error handling follows the WMS spec: parser and pipeline failures are
//! converted to a `<ServiceException>` XML document with HTTP 200, because
//! WMS clients (most notably QGIS and OpenLayers) sniff the *body*, not
//! the HTTP status, when deciding whether a response is an exception.

use std::sync::Arc;

use axum::extract::{RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::pipeline::wms_assemble::serve_get_map;
use crate::protocol::wms::capabilities::generate as generate_capabilities;
use crate::protocol::wms::exception::service_exception;
use crate::protocol::wms::request::{parse, WmsRequest, WmsVersion};
use crate::server::AppState;

/// Base URL used in `<OnlineResource>` xlink hrefs. The HTTP layer doesn't
/// have a real public URL to inject yet (no reverse-proxy headers parsed,
/// no `server.base_url` config field) — for v1 we emit a self-referential
/// placeholder that clients can rewrite if needed. Capabilities consumers
/// almost always concatenate this with `?REQUEST=...` themselves.
fn base_url() -> &'static str {
    "/wms"
}

/// `GET /wms` — entry point.
pub async fn handle(
    State(app): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
) -> Response {
    let q = query.unwrap_or_default();
    match parse(&q) {
        Ok(req) => dispatch(app, req).await,
        Err(e) => {
            // Best-effort version sniff so the exception envelope matches
            // what the client asked for; fall back to 1.3.0 (the newer of
            // the two supported versions).
            let version = sniff_version(&q).unwrap_or(WmsVersion::V1_3_0);
            xml_response(
                service_exception("InvalidParameterValue", &format!("{e:#}"), version),
                StatusCode::OK,
            )
        }
    }
}

async fn dispatch(app: Arc<AppState>, req: WmsRequest) -> Response {
    match req {
        WmsRequest::GetCapabilities { version } => {
            match generate_capabilities(&app.config, version, base_url()) {
                Ok(xml) => xml_response(xml, StatusCode::OK),
                Err(e) => xml_response(
                    service_exception(
                        "NoApplicableCode",
                        &format!("failed to render capabilities: {e:#}"),
                        version,
                    ),
                    StatusCode::OK,
                ),
            }
        }
        WmsRequest::GetMap(map_req) => {
            let version = map_req.version;
            let format = map_req.format;
            match serve_get_map(&app.sources, &app.cache, &app.config, &map_req).await {
                Ok(bytes) => image_response(bytes, format.mime_type()),
                Err(e) => xml_response(
                    service_exception(
                        "InvalidParameterValue",
                        &format!("GetMap failed: {e:#}"),
                        version,
                    ),
                    StatusCode::OK,
                ),
            }
        }
        WmsRequest::GetFeatureInfo(info) => {
            // v1 scope: GetFeatureInfo is parsed (so capabilities can
            // advertise it correctly) but not actually serviced. Return a
            // ServiceException with the OperationNotSupported code — many
            // clients still display it usefully, and the version-matched
            // envelope keeps spec-conformance checkers happy.
            let version = info.get_map.version;
            xml_response(
                service_exception(
                    "OperationNotSupported",
                    "GetFeatureInfo is not implemented in this build (v1)",
                    version,
                ),
                StatusCode::OK,
            )
        }
    }
}

/// Pull the `VERSION=` value out of the raw query without going through
/// the full parser. Used purely so error envelopes round-trip the version
/// the client asked for.
fn sniff_version(query: &str) -> Option<WmsVersion> {
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k.eq_ignore_ascii_case("version") {
                return match v {
                    "1.1.1" => Some(WmsVersion::V1_1_1),
                    "1.3.0" => Some(WmsVersion::V1_3_0),
                    _ => None,
                };
            }
        }
    }
    None
}

fn xml_response(body: String, status: StatusCode) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/xml; charset=utf-8")],
        body,
    )
        .into_response()
}

fn image_response(bytes: bytes::Bytes, mime: &'static str) -> Response {
    (StatusCode::OK, [(header::CONTENT_TYPE, mime)], bytes).into_response()
}
