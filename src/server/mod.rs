//! Axum app — application state, routes, middleware.

pub mod middleware;
pub mod routes;
pub mod state;

pub use self::state::AppState;

use std::sync::Arc;

use axum::routing::get;
use axum::Router;

/// Build the full app router with all routes, state, and middleware
/// attached. See `docs/src/http-server.md` for the endpoint catalog.
///
/// Middleware order matters here: the outermost layer is the first to
/// observe a request, and we want the request-id available everywhere
/// downstream (logs, metrics, handlers), so it goes first. Then metrics
/// (latency stopwatch — must include the handler), then the tower-http
/// tracing layer (spans wrap the request and read the request id from
/// extensions).
pub fn router(state: Arc<AppState>) -> Router {
    // Eagerly install the Prometheus recorder so the first `/metrics`
    // request has a populated handle, and so handlers can emit metrics
    // from the very first request without racing the recorder install.
    let _ = routes::metrics::install();

    Router::new()
        // WMS
        .route("/wms", get(routes::wms::handle))
        // WMTS
        .route("/wmts", get(routes::wmts::handle_kvp))
        .route("/wmts/*rest", get(routes::wmts::handle_rest))
        // TMS
        .route("/tms/*rest", get(routes::tms::handle))
        // health
        .route("/healthz", get(routes::health::healthz))
        .route("/readyz", get(routes::health::readyz))
        // metrics
        .route("/metrics", get(routes::metrics::handler))
        .with_state(state)
        .layer(axum::middleware::from_fn(middleware::request_id::middleware))
        .layer(axum::middleware::from_fn(middleware::metrics::middleware))
        .layer(middleware::tracing::layer())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use opendal::{services, Operator};
    use tower::ServiceExt;

    use crate::cache::hot::HotTier;
    use crate::cache::layered::LayeredCache;
    use crate::cache::persistent::PersistentTier;
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
        }))
    }

    fn test_layer() -> LayerConfig {
        LayerConfig {
            name: "ortho".into(),
            grids: vec!["webmercator".into()],
            max_resolution: MaxResolution::default(),
            bbox: Some(Bbox::from([3.0, 50.0, 7.0, 54.0])),
            sources: vec![SourceConfig {
                uri: "memory://test".into(),
                credentials: None,
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
        }
    }

    fn memory_cache() -> LayeredCache {
        let op = Operator::new(services::Memory::default())
            .expect("memory operator")
            .finish();
        LayeredCache::from_parts(HotTier::new(1024 * 1024), PersistentTier::new(op))
    }

    /// Build an `AppState` with no real sources opened — used by tests
    /// that exercise routes which don't need a working GDAL pipeline.
    fn state_without_sources() -> Arc<AppState> {
        let config = Config {
            layers: vec![test_layer()],
            ..Default::default()
        };
        let registry = SourceRegistry::new();
        Arc::new(AppState::for_tests(config, registry, memory_cache(), 0))
    }

    fn state_with_one_source() -> Arc<AppState> {
        let config = Config {
            layers: vec![test_layer()],
            ..Default::default()
        };
        let mut registry = SourceRegistry::new();
        registry.insert("memory://test".into(), dummy_source());
        Arc::new(AppState::for_tests(config, registry, memory_cache(), 1))
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
        let app = router(state_without_sources());
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

    // Silence unused-import warnings on builds that strip dev-deps.
    #[allow(dead_code)]
    fn _ensure_hashmap_import_used() {
        let _: HashMap<String, String> = HashMap::new();
    }
}
