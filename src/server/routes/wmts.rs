//! WMTS 1.0.0 — KVP at `/wmts?…` and RESTful at `/wmts/*rest`.
//!
//! Both bindings: parse (stage 1) → validate against the catalog (stage 2)
//! → dispatch. Request errors are OWS `ExceptionReport`s from
//! [`crate::protocol::wmts::exception`]; pipeline failures go through
//! [`error_reply`] (internal details logged, never returned).

use std::sync::Arc;

use axum::extract::{Extension, Path, RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::pipeline::render::render_tile;
use crate::protocol::error::OgcError;
use crate::protocol::wmts::exception;
use crate::protocol::wmts::request::{parse_kvp, parse_rest, WmtsRequest};
use crate::protocol::wmts::validate::{validate, WmtsQuery};
use crate::server::middleware::request_id::RequestId;
use crate::server::routes::{error_reply, with_retry_after};
use crate::server::AppState;

/// `GET /wmts?...` — KVP binding.
pub async fn handle_kvp(
    State(app): State<Arc<AppState>>,
    Extension(rid): Extension<RequestId>,
    RawQuery(query): RawQuery,
) -> Response {
    serve(&app, parse_kvp(&query.unwrap_or_default()), &rid).await
}

/// `GET /wmts/*rest` — RESTful binding.
pub async fn handle_rest(
    State(app): State<Arc<AppState>>,
    Extension(rid): Extension<RequestId>,
    Path(rest): Path<String>,
) -> Response {
    serve(&app, parse_rest(&rest), &rid).await
}

async fn serve(app: &AppState, parsed: Result<WmtsRequest, OgcError>, rid: &RequestId) -> Response {
    let query = match parsed.and_then(|req| validate(req, &app.catalog)) {
        Ok(q) => q,
        Err(e) => return request_error(&e),
    };
    match query {
        // Rendered once at startup (see `server::state::Documents`).
        WmtsQuery::GetCapabilities => xml_response(app.documents.wmts.clone(), StatusCode::OK),
        WmtsQuery::GetTile(tile) => match render_tile(&app.renderer, &tile).await {
            Ok(bytes) => (
                StatusCode::OK,
                [(header::CONTENT_TYPE, tile.format().mime_type())],
                bytes,
            )
                .into_response(),
            Err(e) => pipeline_error(&e, "GetTile", rid),
        },
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
            exception::exception_report(reply.code, None, &reply.message),
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
