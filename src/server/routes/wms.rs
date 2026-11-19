//! `GET /wms` — WMS 1.1.1 / 1.3.0.
//!
//! parse (stage 1) → validate against the catalog (stage 2) → dispatch.
//!
//! Errors follow the WMS convention: a `<ServiceExceptionReport>` with HTTP
//! 200, because WMS clients (QGIS, OpenLayers) sniff the body rather than
//! the status. The exception is 429 for a full render queue so clients and
//! proxies back off.

use std::sync::Arc;

use axum::extract::{Extension, RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::pipeline::wms_assemble::serve_get_map;
use crate::protocol::error::Param;
use crate::protocol::kvp::Kvp;
use crate::protocol::wms::exception::{self, service_exception};
use crate::protocol::wms::request::{parse, WmsRequest, WmsVersion};
use crate::protocol::wms::validate::{validate, WmsQuery};
use crate::server::middleware::request_id::RequestId;
use crate::server::routes::{error_reply, with_retry_after};
use crate::server::AppState;

pub async fn handle(
    State(app): State<Arc<AppState>>,
    Extension(rid): Extension<RequestId>,
    RawQuery(query): RawQuery,
) -> Response {
    let raw = query.unwrap_or_default();
    let req = match parse(&raw) {
        Ok(r) => r,
        // Unparseable request: answer in the version the client asked for,
        // if we can tell, else 1.3.0.
        Err(e) => return request_error(&e, sniff_version(&raw).unwrap_or(WmsVersion::V1_3_0)),
    };
    let version = request_version(&req);
    let query = match validate(req, &app.catalog) {
        Ok(q) => q,
        Err(e) => return request_error(&e, version),
    };
    match query {
        // Rendered once at startup (see `server::state::Documents`).
        WmsQuery::GetCapabilities { version } => {
            let doc = match version {
                WmsVersion::V1_1_1 => &app.documents.wms_111,
                WmsVersion::V1_3_0 => &app.documents.wms_130,
            };
            xml_response(doc.clone(), StatusCode::OK)
        }
        WmsQuery::GetMap(map) => match serve_get_map(&app.renderer, &map).await {
            Ok(bytes) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, map.format().mime_type())],
                bytes,
            )
                .into_response(),
            Err(e) => pipeline_error(&e, "GetMap", &rid, version),
        },
    }
}

fn request_version(req: &WmsRequest) -> WmsVersion {
    match req {
        WmsRequest::GetCapabilities { version } | WmsRequest::GetFeatureInfo { version } => *version,
        WmsRequest::GetMap(m) => m.version,
    }
}

/// Best-effort `VERSION=` for error envelopes when the request didn't parse.
fn sniff_version(query: &str) -> Option<WmsVersion> {
    match Kvp::parse(query).ok()?.get(Param::Version)? {
        "1.1.1" => Some(WmsVersion::V1_1_1),
        "1.3.0" => Some(WmsVersion::V1_3_0),
        _ => None,
    }
}

fn request_error(err: &crate::protocol::error::OgcError, version: WmsVersion) -> Response {
    xml_response(exception::for_error(err, version), StatusCode::OK)
}

fn pipeline_error(err: &anyhow::Error, operation: &str, rid: &RequestId, version: WmsVersion) -> Response {
    let reply = error_reply(err, operation, rid);
    let status = if reply.status == StatusCode::TOO_MANY_REQUESTS {
        StatusCode::TOO_MANY_REQUESTS
    } else {
        StatusCode::OK
    };
    with_retry_after(
        status,
        xml_response(service_exception(reply.code, None, &reply.message, version), status),
    )
}

fn xml_response(body: String, status: StatusCode) -> Response {
    (status, [(header::CONTENT_TYPE, "text/xml; charset=utf-8")], body).into_response()
}
