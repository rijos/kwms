//! In-process memory tier — a [`moka::future::Cache`] bounded by total bytes.
//!
//! See `docs/src/caching.md` (Storage stack).

use std::time::Duration;

use anyhow::Result;
use moka::future::Cache;

use super::{CacheTier, Entry, TileKey};

/// In-process cache, sized by total bytes (not entry count). Not durable.
pub struct MemoryTier {
    inner: Cache<TileKey, Entry>,
}

impl MemoryTier {
    /// Build a memory tier whose total weight (sum of tile sizes) is bounded
    /// by `max_bytes`.
    ///
    /// Moka's weigher returns `u32`, so individual entries larger than
    /// `u32::MAX` bytes will be clamped to `u32::MAX` — irrelevant for tiles
    /// (which are at most a few MB) but worth noting.
    pub fn new(max_bytes: u64) -> Self {
        let inner = Cache::builder()
            .max_capacity(max_bytes)
            .weigher(|_key: &TileKey, value: &Entry| -> u32 { value.bytes.len().try_into().unwrap_or(u32::MAX) })
            .build();
        Self { inner }
    }
}

#[async_trait::async_trait]
impl CacheTier for MemoryTier {
    fn name(&self) -> &str {
        "memory"
    }

    fn durable(&self) -> bool {
        false
    }

    async fn get(&self, key: &TileKey, ttl: Duration) -> Result<Option<Entry>> {
        match self.inner.get(key).await {
            Some(entry) if entry.expired(ttl) => {
                self.inner.invalidate(key).await;
                Ok(None)
            }
            other => Ok(other),
        }
    }

    async fn put(&self, key: &TileKey, entry: Entry) -> Result<()> {
        self.inner.insert(key.clone(), entry).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::OutputFormat;
    use crate::tile::TileCoord;
    use bytes::Bytes;

    fn key(z: u32, x: u32, y: u32) -> TileKey {
        TileKey { layer: "L".into(), coord: TileCoord::new(z, x, y), format: OutputFormat::Png, grid_dir: "3857".into() }
    }

    const TTL: Duration = Duration::from_secs(60);

    #[tokio::test]
    async fn put_get_round_trips() {
        let tier = MemoryTier::new(1024);
        let e = Entry::now(Bytes::from_static(b"hello world"));
        tier.put(&key(0, 0, 0), e.clone()).await.unwrap();
        assert_eq!(tier.get(&key(0, 0, 0), TTL).await.unwrap(), Some(e));
        assert!(tier.get(&key(1, 2, 3), TTL).await.unwrap().is_none());
    }

    /// The layer's TTL applies here too (the old hot tier kept tiles until
    /// evicted, however stale).
    #[tokio::test]
    async fn expired_entries_are_misses() {
        let tier = MemoryTier::new(1024);
        let old = Entry { bytes: Bytes::from_static(b"x"), written_at: Entry::now(Bytes::new()).written_at - 120 };
        tier.put(&key(0, 0, 0), old).await.unwrap();
        assert!(tier.get(&key(0, 0, 0), TTL).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn bounded_eviction_keeps_total_weight_under_limit() {
        // 10 entries, 100 bytes each = 1000 bytes total, cap = 300 bytes.
        let cap: u64 = 300;
        let tier = MemoryTier::new(cap);
        for i in 0..10 {
            tier.put(&key(0, i, 0), Entry::now(Bytes::from(vec![0u8; 100]))).await.unwrap();
        }
        tier.inner.run_pending_tasks().await;
        let weighted = tier.inner.weighted_size();
        assert!(weighted <= cap, "weighted_size {weighted} exceeded cap {cap}");
        assert!(tier.inner.entry_count() > 0, "all entries evicted");
    }
}
