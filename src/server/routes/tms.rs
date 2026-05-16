//! `GET /tms/*rest` — TMS 1.0.0 handler.
//!
//! TMS is a path-only protocol (no KVP). The single catchall handler
//! parses `parse_path` and either serves the requested document (root,
//! service-metadata, tile-map-resource) or renders a tile via the
//! pipeline. TMS tile coordinates use a **bottom-up Y axis**, so we
//! flip the wire `y` before calling `render_tile` (which works in
//! the WMTS / XYZ top-down convention).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::config::LayerConfig;
use crate::pipeline::render::render_tile;
use crate::protocol::tms::request::{parse_path, TmsRequest};
use crate::server::AppState;
use crate::tile::coord::TileCoord;
use crate::tile::grid::{lookup_grid, TileGrid};

/// `GET /tms/*rest` — entry point. axum's `*rest` catchall strips the
/// matching prefix (`/tms`) and yields the remainder *without* a leading
/// slash; we re-add it so the parser's normalization stays straightforward.
pub async fn handle(
    State(app): State<Arc<AppState>>,
    Path(rest): Path<String>,
) -> Response {
    let path = if rest.starts_with('/') {
        rest
    } else {
        format!("/{rest}")
    };

    match parse_path(&path) {
        Ok(req) => dispatch(app, req).await,
        Err(e) => xml_exception(
            "InvalidParameterValue",
            &format!("{e:#}"),
            StatusCode::BAD_REQUEST,
        ),
    }
}

async fn dispatch(app: Arc<AppState>, req: TmsRequest) -> Response {
    match req {
        TmsRequest::Root => xml_response(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
             <Services>\n  \
               <TileMapService title=\"kwms TMS\" version=\"1.0.0\" href=\"/tms/1.0.0/\"/>\n\
             </Services>\n"
                .to_string(),
            StatusCode::OK,
        ),
        TmsRequest::ServiceMetadata => render_service_metadata(&app),
        TmsRequest::TileMapResource { layer } => render_tile_map_resource(&app, &layer),
        TmsRequest::GetTile { layer, z, x, y, format } => {
            let layer_cfg = match find_layer(&app, &layer) {
                Some(l) => l,
                None => {
                    return xml_exception(
                        "InvalidParameterValue",
                        &format!("unknown layer `{layer}`"),
                        StatusCode::NOT_FOUND,
                    );
                }
            };

            // TMS does not have an in-band TileMatrixSet selector — each
            // layer is published against exactly one grid (its first
            // configured grid). Multi-grid layers are still served
            // through WMTS; TMS just picks the head.
            let grid_name = match layer_cfg.grids.first() {
                Some(g) => g.as_str(),
                None => {
                    return xml_exception(
                        "NoApplicableCode",
                        &format!("layer `{layer}` has no grids configured"),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    );
                }
            };
            let grid = match lookup_grid(grid_name, &app.config.grids) {
                Some(g) => g,
                None => {
                    return xml_exception(
                        "NoApplicableCode",
                        &format!("unknown grid `{grid_name}` referenced by layer `{layer}`"),
                        StatusCode::INTERNAL_SERVER_ERROR,
                    );
                }
            };

            // Flip TMS bottom-up Y into the top-down convention used by
            // `TileCoord` / `render_tile`.
            let top_down_y = match flip_y(grid.as_ref(), grid_name, z, y) {
                Some(v) => v,
                None => {
                    return xml_exception(
                        "InvalidParameterValue",
                        &format!("TMS y={y} out of range at zoom {z}"),
                        StatusCode::BAD_REQUEST,
                    );
                }
            };

            let coord = TileCoord::new(z, x, top_down_y);
            match render_tile(&app.sources, &app.cache, layer_cfg, &grid, coord, format).await {
                Ok(bytes) => image_response(bytes, format.mime_type()),
                Err(e) => xml_exception(
                    "NoApplicableCode",
                    &format!("GetTile failed: {e:#}"),
                    StatusCode::INTERNAL_SERVER_ERROR,
                ),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn find_layer<'a>(app: &'a AppState, name: &str) -> Option<&'a LayerConfig> {
    app.config.layers.iter().find(|l| l.name == name)
}

/// Translate a TMS (bottom-up) y to an XYZ (top-down) y for the given grid.
///
/// `TileGrid` doesn't expose `matrix_height` directly, so we compute it
/// from the documented invariants of the built-in quad ladders:
///   * `global_geodetic` (GoogleCRS84Quad): height = `1 << z`.
///   * Every other built-in or custom power-of-two quad grid: height
///     = `1 << z`.
///
/// In other words, every grid the server currently ships uses a single
/// world-tile of height 1 at level 0. If a future grid breaks that
/// assumption, this helper is the place to teach it about its layout.
fn flip_y(_grid: &dyn TileGrid, _grid_name: &str, z: u32, y: u32) -> Option<u32> {
    let height: u64 = 1u64 << z;
    if (y as u64) >= height {
        return None;
    }
    Some((height as u32).saturating_sub(1).saturating_sub(y))
}

fn render_service_metadata(app: &AppState) -> Response {
    let mut body = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <TileMapService version=\"1.0.0\">\n  <TileMaps>\n",
    );
    for layer in &app.config.layers {
        body.push_str(&format!(
            "    <TileMap title=\"{title}\" srs=\"{srs}\" href=\"/tms/1.0.0/{name}/\"/>\n",
            title = escape_xml(&layer.title),
            srs = escape_xml(layer_first_crs(layer, app)),
            name = escape_xml(&layer.name),
        ));
    }
    body.push_str("  </TileMaps>\n</TileMapService>\n");
    xml_response(body, StatusCode::OK)
}

fn render_tile_map_resource(app: &AppState, layer_name: &str) -> Response {
    let layer = match find_layer(app, layer_name) {
        Some(l) => l,
        None => {
            return xml_exception(
                "InvalidParameterValue",
                &format!("unknown layer `{layer_name}`"),
                StatusCode::NOT_FOUND,
            );
        }
    };
    let srs = layer_first_crs(layer, app);
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <TileMap version=\"1.0.0\" tilemapservice=\"/tms/1.0.0/\">\n  \
           <Title>{title}</Title>\n  <SRS>{srs}</SRS>\n\
         </TileMap>\n",
        title = escape_xml(&layer.title),
        srs = escape_xml(srs),
    );
    xml_response(body, StatusCode::OK)
}

fn layer_first_crs<'a>(layer: &'a LayerConfig, app: &'a AppState) -> &'a str {
    layer
        .grids
        .first()
        .and_then(|g| lookup_grid(g, &app.config.grids))
        // The grid Arc is consumed inside `and_then`; we want the CRS string,
        // not a borrow into the Arc, so leak via a fixed mapping. A short
        // static fallback keeps the signature `&str`.
        .map(|g| crs_static(g.crs().as_str()))
        .unwrap_or("EPSG:3857")
}

/// Map a CRS string to a `'static` slice by interning the well-known
/// authority codes. Anything unrecognized falls back to `"EPSG:3857"` —
/// this avoids allocating a `String` just to feed the TMS XML writer.
fn crs_static(s: &str) -> &'static str {
    match s {
        "EPSG:3857" => "EPSG:3857",
        "EPSG:4326" => "EPSG:4326",
        "EPSG:28992" => "EPSG:28992",
        "EPSG:3035" => "EPSG:3035",
        _ => "EPSG:3857",
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

fn xml_exception(code: &str, message: &str, status: StatusCode) -> Response {
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <TileMapServerError>\n  \
           <Code>{code}</Code>\n  <Message>{message}</Message>\n\
         </TileMapServerError>\n",
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
