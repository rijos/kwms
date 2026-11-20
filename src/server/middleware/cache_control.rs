//! `Cache-Control` on every response.
//!
//! A handler that returns something a shared cache may keep — a tile, a
//! GetMap image, a capabilities document — marks the response with
//! [`Cacheable`], carrying the configured value (`server.cache_control`, or
//! the layer's own). Every other response gets `no-store`: errors, WMS
//! exceptions (HTTP 200, so status alone can't tell), overload and panic
//! replies, health checks. A CDN or proxy then never keeps a transient
//! failure.

use axum::body::Body;
use axum::http::{header, HeaderValue, Request};
use axum::middleware::Next;
use axum::response::Response;

/// Response extension: this response may be cached, with this
/// `Cache-Control` value (`None`: send no header).
#[derive(Debug, Clone)]
pub struct Cacheable(pub Option<HeaderValue>);

/// Mark `response` cacheable with `value`.
pub fn cacheable(mut response: Response, value: Option<&HeaderValue>) -> Response {
    response.extensions_mut().insert(Cacheable(value.cloned()));
    response
}

pub async fn middleware(req: Request<Body>, next: Next) -> Response {
    let mut response = next.run(req).await;
    match response.extensions_mut().remove::<Cacheable>() {
        Some(Cacheable(Some(value))) => {
            response.headers_mut().insert(header::CACHE_CONTROL, value);
        }
        Some(Cacheable(None)) => {}
        None => {
            response
                .headers_mut()
                .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        }
    }
    response
}

/// Parsed `Cache-Control` values, built once at startup from the config.
#[derive(Debug, Clone, Default)]
pub struct CacheControl {
    images: std::collections::HashMap<String, Option<HeaderValue>>,
    capabilities: Option<HeaderValue>,
}

impl CacheControl {
    pub fn from_config(config: &crate::config::Config) -> anyhow::Result<Self> {
        let parse = |v: &str| -> anyhow::Result<Option<HeaderValue>> {
            if v.trim().is_empty() {
                return Ok(None);
            }
            Ok(Some(HeaderValue::from_str(v.trim()).map_err(|_| {
                anyhow::anyhow!("'{v}' is not a valid Cache-Control value")
            })?))
        };
        let server = &config.server.cache_control;
        let mut images = std::collections::HashMap::new();
        for layer in &config.layers {
            let value = layer.cache_control.as_deref().unwrap_or(&server.images);
            images.insert(layer.name.clone(), parse(value)?);
        }
        Ok(Self {
            images,
            capabilities: parse(&server.capabilities)?,
        })
    }

    /// For an image of `layer`.
    pub fn image(&self, layer: &str) -> Option<&HeaderValue> {
        self.images.get(layer).and_then(Option::as_ref)
    }

    /// For a capabilities/metadata document.
    pub fn capabilities(&self) -> Option<&HeaderValue> {
        self.capabilities.as_ref()
    }
}
