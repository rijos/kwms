//! Source registry — id-to-`Source` lookup owned by `AppState`.

use std::collections::HashMap;
use std::sync::Arc;

use super::Source;

#[derive(Default)]
pub struct SourceRegistry {
    sources: HashMap<String, Arc<dyn Source>>,
}

impl SourceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn Source>> {
        self.sources.get(id).cloned()
    }

    pub fn insert(&mut self, id: String, source: Arc<dyn Source>) {
        self.sources.insert(id, source);
    }
}
