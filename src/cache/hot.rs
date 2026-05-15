//! In-process "hot" tier — a [`moka::future::Cache`] bounded by total bytes.
//!
//! See `docs/src/caching.md` (Storage stack).

use bytes::Bytes;
use moka::future::Cache;

use super::TileKey;

/// Hot in-process cache, sized by total bytes (not entry count).
pub struct HotTier {
    inner: Cache<TileKey, Bytes>,
}

impl HotTier {
    /// Build a hot tier whose total weight (sum of `bytes.len()`) is bounded
    /// by `max_bytes`.
    ///
    /// Moka's weigher returns `u32`, so individual entries larger than
    /// `u32::MAX` bytes will be clamped to `u32::MAX` — irrelevant for tiles
    /// (which are at most a few MB) but worth noting.
    pub fn new(max_bytes: u64) -> Self {
        let inner = Cache::builder()
            .max_capacity(max_bytes)
            .weigher(|_key: &TileKey, value: &Bytes| -> u32 {
                value.len().try_into().unwrap_or(u32::MAX)
            })
            .build();
        Self { inner }
    }

    pub async fn get(&self, key: &TileKey) -> Option<Bytes> {
        self.inner.get(key).await
    }

    pub async fn put(&self, key: TileKey, bytes: Bytes) {
        self.inner.insert(key, bytes).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crs::Crs;
    use crate::encode::OutputFormat;
    use crate::tile::TileCoord;

    fn key(z: u32, x: u32, y: u32) -> TileKey {
        TileKey {
            layer: "L".into(),
            coord: TileCoord::new(z, x, y),
            format: OutputFormat::Png,
            crs: Crs::new("EPSG:3857"),
        }
    }

    #[tokio::test]
    async fn put_get_round_trips() {
        let tier = HotTier::new(1024);
        let k = key(0, 0, 0);
        let v = Bytes::from_static(b"hello world");
        tier.put(k.clone(), v.clone()).await;
        let got = tier.get(&k).await.expect("present");
        assert_eq!(got, v);
    }

    #[tokio::test]
    async fn missing_key_is_none() {
        let tier = HotTier::new(1024);
        assert!(tier.get(&key(1, 2, 3)).await.is_none());
    }

    #[tokio::test]
    async fn bounded_eviction_keeps_total_weight_under_limit() {
        // 10 entries, 100 bytes each = 1000 bytes total, cap = 300 bytes.
        let cap: u64 = 300;
        let tier = HotTier::new(cap);
        let payload = Bytes::from(vec![0u8; 100]);
        for i in 0..10 {
            tier.put(key(0, i, 0), payload.clone()).await;
        }
        // Force moka to settle pending eviction work.
        tier.inner.run_pending_tasks().await;

        let weighted = tier.inner.weighted_size();
        assert!(
            weighted <= cap,
            "weighted_size {weighted} exceeded cap {cap}"
        );
        // And some entries must have actually been kept.
        assert!(tier.inner.entry_count() > 0, "all entries evicted");
    }
}
