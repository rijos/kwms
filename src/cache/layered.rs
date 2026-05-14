//! Two-tier cache (hot + persistent) with single-flight coalescing.

use anyhow::Result;
use bytes::Bytes;

use super::TileKey;

pub struct LayeredCache {
    // Hot tier (moka), persistent tier (opendal), and single-flight map go here.
}

impl LayeredCache {
    pub async fn get(&self, _key: &TileKey) -> Result<Option<Bytes>> {
        todo!("phase 2 (cache agent)")
    }

    pub async fn put(&self, _key: &TileKey, _bytes: Bytes) -> Result<()> {
        todo!("phase 2 (cache agent)")
    }
}
