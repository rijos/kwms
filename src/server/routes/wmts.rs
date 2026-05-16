//! WMTS 1.0.0 handlers — KVP at `/wmts?...` and RESTful at `/wmts/*rest`.
//!
//! Both bindings parse to the same [`WmtsRequest`] enum and dispatch
//! through `serve_request`. Errors are returned as `text/xml` documents
//! (an OWS-style exception, kept inline to avoid pulling in an extra
//! module for what is two short error envelopes).

use std::sync::Arc;

use axum::extract::{Path, RawQuery, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::config::LayerConfig;
use crate::pipeline::render::render_tile;
use crate::protocol::wmts::capabilities::generate as generate_capabilities;
use crate::protocol::wmts::request::{parse_kvp, parse_rest, GetTileRequest, WmtsRequest};
use crate::server::AppState;
use crate::tile::coord::TileCoord;
use crate::tile::grid::{lookup_grid, TileGrid};

/// Base URL used in `<ResourceURL>` templates for WMTS capabilities.
/// See [`crate::server::routes::wms::base_url`] for the rationale on
/// using a relative placeholder in v1.
fn base_url() -> &'static str {
    ""
}

/// `GET /wmts?...` — KVP binding.
pub async fn handle_kvp(
    State(app): State<Arc<AppState>>,
    RawQuery(query): RawQuery,
) -> Response {
    let q = query.unwrap_or_default();
    match parse_kvp(&q) {
        Ok(req) => dispatch(app, req).await,
        Err(e) => xml_exception(
            "InvalidParameterValue",
            &format!("{e:#}"),
            StatusCode::BAD_REQUEST,
        ),
    }
}

/// `GET /wmts/*rest` — RESTful binding.
pub async fn handle_rest(
    State(app): State<Arc<AppState>>,
    Path(rest): Path<String>,
) -> Response {
    // axum's `*rest` catchall strips the matching prefix and yields the
    // remaining path *without* a leading slash; `parse_rest` tolerates
    // either spelling.
    match parse_rest(&rest) {
        Ok(req) => dispatch(app, req).await,
        Err(e) => xml_exception(
            "InvalidParameterValue",
            &format!("{e:#}"),
            StatusCode::BAD_REQUEST,
        ),
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

async fn dispatch(app: Arc<AppState>, req: WmtsRequest) -> Response {
    match req {
        WmtsRequest::GetCapabilities => match generate_capabilities(&app.config, base_url()) {
            Ok(xml) => xml_response(xml, StatusCode::OK),
            Err(e) => xml_exception(
                "NoApplicableCode",
                &format!("failed to render capabilities: {e:#}"),
                StatusCode::INTERNAL_SERVER_ERROR,
            ),
        },
        WmtsRequest::GetTile(tile_req) => serve_get_tile(app, tile_req).await,
        WmtsRequest::GetFeatureInfo(_) => xml_exception(
            "OperationNotSupported",
            "WMTS GetFeatureInfo is not implemented in this build (v1)",
            StatusCode::NOT_IMPLEMENTED,
        ),
    }
}

async fn serve_get_tile(app: Arc<AppState>, req: GetTileRequest) -> Response {
    // 1. Locate the layer.
    let layer = match find_layer(&app, &req.layer) {
        Some(l) => l,
        None => {
            return xml_exception(
                "InvalidParameterValue",
                &format!("unknown layer `{}`", req.layer),
                StatusCode::NOT_FOUND,
            );
        }
    };

    // 2. Locate the grid. The KVP spelling uses the canonical WMTS
    //    TileMatrixSet identifier (e.g. `GoogleMapsCompatible`); the
    //    config layer references it under kwms's grid name
    //    (`webmercator`). Try both.
    let grid = match resolve_grid_for_layer(layer, &app, &req.tile_matrix_set) {
        Some(g) => g,
        None => {
            return xml_exception(
                "InvalidParameterValue",
                &format!(
                    "layer `{}` is not advertised on tile matrix set `{}`",
                    req.layer, req.tile_matrix_set
                ),
                StatusCode::BAD_REQUEST,
            );
        }
    };

    // 3. Parse the matrix identifier as a level. Capabilities advertises
    //    levels as base-10 integers (`"0"`, `"1"`, ...).
    let z: u32 = match req.tile_matrix.parse() {
        Ok(z) => z,
        Err(_) => {
            return xml_exception(
                "InvalidParameterValue",
                &format!("invalid TileMatrix `{}` (expected integer)", req.tile_matrix),
                StatusCode::BAD_REQUEST,
            );
        }
    };

    let coord = TileCoord::new(z, req.tile_col, req.tile_row);
    match render_tile(&app.sources, &app.cache, layer, &grid, coord, req.format).await {
        Ok(bytes) => image_response(bytes, req.format.mime_type()),
        Err(e) => xml_exception(
            "NoApplicableCode",
            &format!("GetTile failed: {e:#}"),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

// ---------------------------------------------------------------------------
// Lookups
// ---------------------------------------------------------------------------

fn find_layer<'a>(app: &'a AppState, name: &str) -> Option<&'a LayerConfig> {
    app.config.layers.iter().find(|l| l.name == name)
}

/// Resolve the [`TileGrid`] used to render the requested TileMatrixSet
/// against the named layer.
///
/// WMTS clients address grids by their *capabilities* name (e.g.
/// `GoogleMapsCompatible`), but kwms's config addresses them by their
/// kwms id (e.g. `webmercator`). Map between the two using the same
/// table the capabilities generator uses, then look the kwms id up
/// against the layer's `grids` list before instantiating.
fn resolve_grid_for_layer(
    layer: &LayerConfig,
    app: &AppState,
    tms_name: &str,
) -> Option<Arc<dyn TileGrid>> {
    let kwms_name = kwms_name_for_tms(tms_name);
    if !layer.grids.iter().any(|g| g == kwms_name || g == tms_name) {
        return None;
    }
    // Try the kwms name first (covers all the well-known grids and any
    // custom grid that didn't get a `tms_name` mapping); fall back to the
    // wire name in case a custom grid was declared under its TMS spelling.
    lookup_grid(kwms_name, &app.config.grids)
        .or_else(|| lookup_grid(tms_name, &app.config.grids))
}

/// Inverse of the table in [`crate::protocol::wmts::capabilities`] — map a
/// canonical WMTS TileMatrixSet name back to its kwms grid id.
fn kwms_name_for_tms(tms: &str) -> &str {
    match tms {
        "GoogleMapsCompatible" => "webmercator",
        "GoogleCRS84Quad" => "global_geodetic",
        "NetherlandsRDNewQuad" => "rd_new_quad",
        "EuropeanETRS89_LAEAQuad" => "etrs89_laea_quad",
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Response shaping
// ---------------------------------------------------------------------------

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

/// Minimal OWS-style ExceptionReport. Inline because WMTS errors are rare
/// enough that a dedicated module would be overkill.
fn xml_exception(code: &str, message: &str, status: StatusCode) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <ExceptionReport xmlns=\"http://www.opengis.net/ows/1.1\" version=\"1.1.0\">\n\
           <Exception exceptionCode=\"{code}\">\n\
             <ExceptionText>{message}</ExceptionText>\n\
           </Exception>\n\
         </ExceptionReport>\n",
        code = escape_xml(code),
        message = escape_xml(message),
    );
    xml_response(body, status)
}

fn escape_xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
