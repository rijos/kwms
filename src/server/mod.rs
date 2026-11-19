//! Axum app — application state, routes, middleware.

pub mod middleware;
pub mod routes;
pub mod state;

pub use self::state::AppState;

use std::sync::Arc;

use axum::error_handling::HandleErrorLayer;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{BoxError, Router};
use tower::limit::GlobalConcurrencyLimitLayer;
use tower::ServiceBuilder;
use tower_http::catch_panic::CatchPanicLayer;

use crate::config::ServerConfig;

/// Build the public app router: protocol routes, health probes, and
/// middleware. See `docs/src/http-server.md` for the endpoint catalog.
/// `/metrics` is deliberately *not* here — it is served by
/// [`metrics_router`] on its own listener (`server.metrics_bind`).
///
/// Protocol routes (WMS/WMTS/TMS) are wrapped in overload protection:
/// at most `server.max_concurrent_requests` in flight (one limit shared
/// across all of them; excess requests are shed immediately with 429) and a
/// `server.request_timeout` budget per request (503 when exceeded). Health
/// probes sit outside those limits so an overloaded server isn't restarted
/// by its orchestrator for failing liveness.
///
/// Middleware order matters here: the outermost layer is the first to
/// observe a request, and we want the request-id available everywhere
/// downstream (logs, metrics, handlers), so it goes first. Then metrics
/// (latency stopwatch — must include the handler), then the tower-http
/// tracing layer (spans wrap the request and read the request id from
/// extensions).
pub fn router(state: Arc<AppState>) -> Router {
    // Install the Prometheus recorder up front so handlers can emit metrics
    // from the very first request, whether or not `/metrics` is served.
    let _ = routes::metrics::install();

    let protocols = Router::new()
        // WMS
        .route("/wms", get(routes::wms::handle))
        // WMTS: KVP at `wmts.path`, RESTful below it (validated config).
        .route(&state.config.wmts.path, get(routes::wmts::handle_kvp))
        .route(&format!("{}/*rest", state.config.wmts.path), get(routes::wmts::handle_rest))
        // TMS
        .route("/tms/*rest", get(routes::tms::handle));
    let protocols = with_overload_protection(protocols, &state.config.server);

    Router::new()
        .merge(protocols)
        // health
        .route("/healthz", get(routes::health::healthz))
        .route("/readyz", get(routes::health::readyz))
        .with_state(state)
        // Innermost: a panic in a handler (e.g. an integer overflow, which
        // panics in every build profile) becomes a generic 500 instead of a
        // dropped connection, and is logged. Inside request-id/metrics/trace
        // so those still see the response.
        .layer(CatchPanicLayer::custom(panic_response))
        .layer(axum::middleware::from_fn(middleware::request_id::middleware))
        .layer(axum::middleware::from_fn(middleware::metrics::middleware))
        .layer(middleware::tracing::layer())
}

/// Response for a handler panic: generic text only (the panic payload can
/// contain anything), with the details logged.
fn panic_response(payload: Box<dyn std::any::Any + Send + 'static>) -> Response {
    let detail = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_default();
    tracing::error!(panic = %detail, "request handler panicked");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal server error").into_response()
}

/// Apply the request timeout and the shared concurrency limit (with load
/// shedding) to every route in `routes`.
fn with_overload_protection<S>(routes: Router<S>, server: &ServerConfig) -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    routes.layer(
        ServiceBuilder::new()
            .layer(HandleErrorLayer::new(overload_response))
            .timeout(server.request_timeout)
            .load_shed()
            .layer(GlobalConcurrencyLimitLayer::new(server.max_concurrent_requests)),
    )
}

/// Router for the metrics listener: `GET /metrics` only.
pub fn metrics_router() -> Router {
    let _ = routes::metrics::install();
    Router::new().route("/metrics", get(routes::metrics::handler))
}

/// Map overload-protection errors to responses. Shedding at the concurrency
/// limit is a capacity signal → 429 (5xx is reserved for real failures); a
/// request that blew its time budget may be a stuck backend → 503.
async fn overload_response(err: BoxError) -> Response {
    let (status, msg) = if err.is::<tower::load_shed::error::Overloaded>() {
        (StatusCode::TOO_MANY_REQUESTS, "server is busy, retry later")
    } else if err.is::<tower::timeout::error::Elapsed>() {
        (StatusCode::SERVICE_UNAVAILABLE, "request timed out")
    } else {
        tracing::error!(error = %err, "unhandled middleware error");
        (StatusCode::SERVICE_UNAVAILABLE, "service unavailable")
    };
    (status, [(header::RETRY_AFTER, "1")], msg).into_response()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use crate::cache::memory::MemoryTier;
    use crate::cache::layered::LayeredCache;
    use crate::config::{
        Config, LayerCacheConfig, LayerConfig, MaxResolution, SourceConfig,
    };
    use crate::crs::Bbox;
    use crate::encode::OutputFormat;
    use crate::source::{PixelType, Source, SourceMetadata, SourceRegistry};

    use super::*;

    // ----- test scaffolding -----

    /// Minimal `Source` impl that satisfies the trait without actually
    /// reading any pixels. Routes that try to call into it will fail,
    /// but for healthz/readyz/capabilities/metrics that's fine.
    struct DummySource(SourceMetadata);

    #[async_trait::async_trait]
    impl Source for DummySource {
        fn metadata(&self) -> &SourceMetadata {
            &self.0
        }
        async fn read_window(
            &self,
            _req: crate::source::WindowRequest,
        ) -> anyhow::Result<crate::source::PixelBuffer> {
            anyhow::bail!("dummy source")
        }
        async fn sample(
            &self,
            _coord: crate::crs::Coord,
            _crs: crate::crs::Crs,
        ) -> anyhow::Result<Vec<crate::source::PixelValue>> {
            anyhow::bail!("dummy source")
        }
    }

    fn dummy_source() -> Arc<dyn Source> {
        Arc::new(DummySource(SourceMetadata {
            native_crs: crate::crs::Crs::new("EPSG:3857"),
            native_bbox: Bbox::from([0.0, 0.0, 1.0, 1.0]),
            band_count: 3,
            pixel_type: PixelType::U8,
            native_resolution: 1.0,
            nodata: None,
            size: (1, 1),
            overviews: Vec::new(),
        }))
    }

    fn test_layer() -> LayerConfig {
        LayerConfig {
            name: "ortho".into(),
            grids: vec!["webmercator".into()],
            max_resolution: MaxResolution::default(),
            bbox_crs: None,
            bbox: Some(Bbox::from([3.0, 50.0, 7.0, 54.0])),
            sources: vec![SourceConfig {
                uri: "memory://test".into(),
                credentials: None,
                ..Default::default()
            }],
            cache: LayerCacheConfig::default(),
            title: "Test Layer".into(),
            abstract_: "Test".into(),
            keywords: vec![],
            attribution: None,
            metadata_url: None,
            formats: vec![OutputFormat::Png],
            resampling: "bilinear".into(),
            queryable: true,
            opaque: false,
            nodata: None,
            source_mask: None,
            output_formats: Default::default(),
        }
    }

    fn memory_cache() -> LayeredCache {
        LayeredCache::from_tiers(vec![std::sync::Arc::new(MemoryTier::new(1024 * 1024))])
    }

    /// Build an `AppState` with no real sources opened — used by tests
    /// that exercise routes which don't need a working GDAL pipeline.
    fn state_without_sources() -> Arc<AppState> {
        let config = Config {
            layers: vec![test_layer()],
            ..Default::default()
        };
        let registry = SourceRegistry::new();
        Arc::new(AppState::from_parts(config, registry, memory_cache(), 0).expect("test state"))
    }

    fn state_with_one_source() -> Arc<AppState> {
        let config = Config {
            layers: vec![test_layer()],
            ..Default::default()
        };
        let mut registry = SourceRegistry::new();
        registry.insert("memory://test".into(), dummy_source());
        Arc::new(AppState::from_parts(config, registry, memory_cache(), 1).expect("test state"))
    }

    async fn body_to_string(resp: axum::response::Response) -> String {
        let body = resp.into_body();
        // Generous limit — capabilities documents can hit tens of KB for
        // multi-layer configs; we only care that the whole body arrives.
        let bytes = axum::body::to_bytes(body, 1024 * 1024)
            .await
            .expect("read response body");
        String::from_utf8_lossy(&bytes).to_string()
    }

    // ----- tests -----

    #[tokio::test]
    async fn healthz_returns_200_ok() {
        let app = router(state_without_sources());
        let resp = app
            .oneshot(Request::builder().uri("/healthz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_to_string(resp).await;
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn readyz_503_when_no_sources() {
        let app = router(state_without_sources());
        let resp = app
            .oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn readyz_200_when_source_registered() {
        let app = router(state_with_one_source());
        let resp = app
            .oneshot(Request::builder().uri("/readyz").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn wms_get_capabilities_returns_xml() {
        let app = router(state_with_one_source());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wms?REQUEST=GetCapabilities&SERVICE=WMS&VERSION=1.3.0")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(ct.contains("xml"), "expected XML content-type, got {ct}");
        let body = body_to_string(resp).await;
        assert!(
            body.contains("<WMS_Capabilities"),
            "missing 1.3.0 root element; body:\n{body}"
        );
    }

    #[tokio::test]
    async fn wmts_get_capabilities_returns_xml() {
        let app = router(state_with_one_source());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wmts?REQUEST=GetCapabilities&SERVICE=WMTS")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = body_to_string(resp).await;
        assert!(
            body.contains("<Capabilities"),
            "missing WMTS root <Capabilities>; body:\n{body}"
        );
    }

    #[tokio::test]
    async fn metrics_endpoint_returns_prometheus_exposition() {
        let app = metrics_router();
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/metrics")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(
            ct.starts_with("text/plain"),
            "expected text/plain content-type, got `{ct}`"
        );
        // We just need the body to be a string — the Prometheus exposition
        // format may legitimately be empty if no metrics have fired yet.
        let _body = body_to_string(resp).await;
    }

    #[tokio::test]
    async fn metrics_is_not_served_on_the_public_router() {
        let app = router(state_without_sources());
        let resp = app
            .oneshot(Request::builder().uri("/metrics").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn handler_panics_become_generic_500s() {
        #[allow(arithmetic_overflow)]
        async fn overflow() -> String {
            let x: u8 = std::hint::black_box(255);
            (x + 1).to_string() // panics: overflow checks are on in tests
        }
        let app = Router::new()
            .route("/boom", get(overflow))
            .layer(CatchPanicLayer::custom(panic_response));
        let resp = app.oneshot(Request::builder().uri("/boom").body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body_to_string(resp).await, "internal server error");
    }

    #[tokio::test]
    async fn client_request_ids_are_never_echoed() {
        // Our id is the only one echoed; a client's (valid or not) is at most logged.
        let app = router(state_without_sources());
        let long = "a".repeat(200);
        for bad in [long.as_str(), "spoofed id with spaces", "x\"<>", "trace-123_abc.DEF"] {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri("/healthz")
                        .header("x-request-id", bad)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            let echoed = resp.headers().get("x-request-id").unwrap().to_str().unwrap();
            assert_ne!(echoed, bad);
            assert_eq!(echoed.len(), 16, "fresh hex id expected, got {echoed}");
            assert!(echoed.bytes().all(|b| b.is_ascii_hexdigit()));
        }
    }

    #[tokio::test]
    async fn get_map_over_pixel_limit_is_a_service_exception_without_internals() {
        let app = router(state_with_one_source());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri(
                        "/wms?SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=ortho&STYLES=&\
                         CRS=EPSG:3857&BBOX=0,0,1000,1000&WIDTH=6000&HEIGHT=6000&FORMAT=image/png",
                    )
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let body = body_to_string(resp).await;
        assert!(body.contains(r#"locator="WIDTH">WIDTH/HEIGHT exceed the maximum image size<"#), "{body}");
    }

    #[tokio::test]
    async fn internal_errors_are_not_leaked_to_clients() {
        // The dummy source fails every read with "dummy source"; a tile
        // inside its coverage reaches that path. Clients must only see a
        // generic message plus the request id.
        let app = router(state_with_one_source());
        let resp = app
            .oneshot(
                Request::builder()
                    .uri("/wmts/ortho/GoogleMapsCompatible/0/0/0.png")
                    .header("x-request-id", "req-42")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let ours = resp.headers()["x-request-id"].to_str().unwrap().to_string();
        let body = body_to_string(resp).await;
        assert!(!body.contains("dummy source"), "internal detail leaked: {body}");
        assert!(body.contains(&ours), "our request id missing: {body}");
        assert!(!body.contains("req-42"), "client request id echoed: {body}");
    }

    /// A router whose `/slow` handler signals `started` once it is running
    /// (i.e. admitted), then takes 300 ms.
    fn slow_router_with_signal(server: &ServerConfig, started: Arc<tokio::sync::Notify>) -> Router {
        let routes = Router::new().route(
            "/slow",
            get(move || {
                let started = started.clone();
                async move {
                    started.notify_one();
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    "done"
                }
            }),
        );
        with_overload_protection(routes, server)
    }

    fn slow_router(server: &ServerConfig) -> Router {
        slow_router_with_signal(server, Arc::new(tokio::sync::Notify::new()))
    }

    fn get_req(uri: &str) -> Request<Body> {
        Request::builder().uri(uri).body(Body::empty()).unwrap()
    }

    #[tokio::test]
    async fn requests_over_the_concurrency_limit_are_shed() {
        let server = ServerConfig {
            max_concurrent_requests: 1,
            ..ServerConfig::default()
        };
        let started = Arc::new(tokio::sync::Notify::new());
        let app = slow_router_with_signal(&server, started.clone());
        let first = tokio::spawn(app.clone().oneshot(get_req("/slow")));
        // Proceed only once the first request holds the only slot.
        started.notified().await;
        let second = app.clone().oneshot(get_req("/slow")).await.unwrap();
        assert_eq!(second.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body_to_string(second).await, "server is busy, retry later");
        assert_eq!(first.await.unwrap().unwrap().status(), StatusCode::OK);
        // The slot is released afterwards.
        assert_eq!(app.oneshot(get_req("/slow")).await.unwrap().status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn requests_over_the_time_budget_get_503() {
        let server = ServerConfig {
            request_timeout: std::time::Duration::from_millis(50),
            ..ServerConfig::default()
        };
        let resp = slow_router(&server).oneshot(get_req("/slow")).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_to_string(resp).await, "request timed out");
    }

    // Silence unused-import warnings on builds that strip dev-deps.
    #[allow(dead_code)]
    fn _ensure_hashmap_import_used() {
        let _: HashMap<String, String> = HashMap::new();
    }
}
