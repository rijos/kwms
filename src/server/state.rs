//! Application state shared by all axum handlers.

use std::sync::Arc;

use crate::cache::LayeredCache;
use crate::config::Config;
use crate::source::SourceRegistry;

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<Config>,
    pub sources: Arc<SourceRegistry>,
    pub cache: Arc<LayeredCache>,
}
