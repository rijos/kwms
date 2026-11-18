//! `GET /tms/*rest` — TMS 1.0.0.
//!
//! parse ([`parse_path`]) → validate against the catalog ([`validate`]) →
//! dispatch. Request errors are reported with
//! [`crate::protocol::tms::exception`]; pipeline failures via
//! [`error_reply`] (internal details logged, never returned).

use std::sync::Arc;

use axum::extract::{Extension, Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::pipeline::render::render_tile;
use crate::protocol::error::OgcError;
use crate::protocol::tms::exception;
use crate::protocol::tms::request::parse_path;
use crate::protocol::tms::validate::{validate, TmsQuery};
use crate::server::middleware::request_id::RequestId;
use crate::server::routes::{error_reply, with_retry_after};
use crate::server::AppState;

pub async fn handle(
    State(app): State<Arc<AppState>>,
    Extension(rid): Extension<RequestId>,
    Path(rest): Path<String>,
) -> Response {
    let query = match parse_path(&rest).and_then(|req| validate(req, &app.catalog)) {
        Ok(q) => q,
        Err(e) => return request_error(&e),
    };
    // Documents are rendered once at startup (see `server::state::Documents`).
    let doc = match query {
        TmsQuery::Root => Some(app.documents.tms_root.clone()),
        TmsQuery::ServiceMetadata => Some(app.documents.tms_service.clone()),
        TmsQuery::TileMap(layer) => app.documents.tms_layers.get(layer.name()).cloned(),
        TmsQuery::GetTile(tile) => {
            return match render_tile(&app.renderer, &tile).await {
                Ok(bytes) => (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, tile.format().mime_type())],
                    bytes,
                )
                    .into_response(),
                Err(e) => pipeline_error(&e, "GetTile", &rid),
            };
        }
    };
    match doc {
        Some(xml) => xml_response(xml, StatusCode::OK),
        None => pipeline_error(&anyhow::anyhow!("internal: no TileMap document for a catalog layer"), "TMS document", &rid),
    }
}

fn request_error(err: &OgcError) -> Response {
    let (status, xml) = exception::for_error(err);
    xml_response(
        xml,
        StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
    )
}

fn pipeline_error(err: &anyhow::Error, operation: &str, rid: &RequestId) -> Response {
    let reply = error_reply(err, operation, rid);
    with_retry_after(
        reply.status,
        xml_response(
            exception::error_document(reply.code, &reply.message),
            reply.status,
        ),
    )
}

fn xml_response(body: String, status: StatusCode) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "text/xml; charset=utf-8")],
        body,
    )
        .into_response()
}
