//! Two-tier cache (hot + persistent) with single-flight coalescing.
//!
//! See `docs/src/caching.md` (Stampede protection).
//!
//! # Single-flight algorithm
//!
//! For each `key`, at most one `compute` runs at a time across this process.
//! Concurrent callers wait on a per-key [`tokio::sync::Notify`] and recheck
//! the cache after they wake.
//!
//! ```text
//! get_or_compute(key, ttl, compute):
//!   1. Fast path: if `get(key, ttl)` hits, return immediately.
//!   2. Take the inflight map lock briefly:
//!      - If another task already registered a Notify for `key`, clone it,
//!        drop the map lock, `notified().await`, and go back to step 1.
//!      - Otherwise insert a new Notify and become the leader.
//!   3. Leader double-checks the cache (a peer could have populated it
//!      between step 1 and step 2).
//!   4. Leader runs `compute`. On success it `put`s the bytes into both
//!      tiers and on Ok/Err it removes the Notify from the inflight map
//!      and `notify_waiters()` so followers can recheck the cache.
//! ```
//!
//! The Notify pattern is "best-effort wake": followers always recheck the
//! cache after waking and may legitimately fall through to becoming the
//! next leader if the previous leader's `compute` failed.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use bytesize::ByteSize;

use super::hot::HotTier;
use super::persistent::{build_operator, PersistentTier};
use super::TileKey;
use crate::config::model::{Credentials, GlobalCacheConfig};

pub struct LayeredCache {
    hot: HotTier,
    persistent: PersistentTier,
    inflight: tokio::sync::Mutex<HashMap<TileKey, Arc<tokio::sync::Notify>>>,
}

impl LayeredCache {
    pub fn new(
        cfg: &GlobalCacheConfig,
        credentials: &HashMap<String, Credentials>,
    ) -> Result<Self> {
        let max_bytes: u64 = cfg
            .hot
            .max_bytes
            .parse::<ByteSize>()
            .map_err(|e| anyhow!("invalid cache.hot.max_bytes `{}`: {e}", cfg.hot.max_bytes))?
            .as_u64();
        let hot = HotTier::new(max_bytes);
        let op = build_operator(&cfg.persistent, credentials)?;
        let persistent = PersistentTier::new(op);
        Ok(Self {
            hot,
            persistent,
            inflight: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Construct from already-built tiers — useful for tests and callers
    /// that have a custom `Operator` (e.g. wrapped in layers).
    pub fn from_parts(hot: HotTier, persistent: PersistentTier) -> Self {
        Self {
            hot,
            persistent,
            inflight: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Look up `key`. Checks the hot tier first, then the persistent tier
    /// (with TTL). On a persistent-tier hit, the value is promoted to the
    /// hot tier.
    pub async fn get(&self, key: &TileKey, ttl: Duration) -> Result<Option<Bytes>> {
        if let Some(b) = self.hot.get(key).await {
            return Ok(Some(b));
        }
        if let Some(b) = self.persistent.get(key, ttl).await? {
            // Promote into the hot tier.
            self.hot.put(key.clone(), b.clone()).await;
            return Ok(Some(b));
        }
        Ok(None)
    }

    /// Store `bytes` in both tiers. Errors from the persistent tier are
    /// surfaced; the hot tier insertion is unconditional.
    pub async fn put(&self, key: &TileKey, bytes: Bytes, _ttl: Duration) -> Result<()> {
        self.hot.put(key.clone(), bytes.clone()).await;
        self.persistent.put(key, bytes).await?;
        Ok(())
    }

    /// Get-or-compute with per-key single-flight. See the module docs for the
    /// algorithm.
    pub async fn get_or_compute<F, Fut>(
        &self,
        key: TileKey,
        ttl: Duration,
        compute: F,
    ) -> Result<Bytes>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Bytes>>,
    {
        // 1. Fast path.
        if let Some(b) = self.get(&key, ttl).await? {
            return Ok(b);
        }

        // 2. Register or join the inflight slot.
        let (is_leader, notify) = {
            let mut map = self.inflight.lock().await;
            if let Some(existing) = map.get(&key) {
                (false, existing.clone())
            } else {
                let n = Arc::new(tokio::sync::Notify::new());
                map.insert(key.clone(), n.clone());
                (true, n)
            }
        };

        if !is_leader {
            // Wait to be woken, then loop back and recheck the cache.
            // If the leader's compute failed, the cache will still be a miss
            // and we may end up running compute ourselves on the next call —
            // that's acceptable v1 behavior (no error propagation between
            // single-flight peers).
            notify.notified().await;
            if let Some(b) = self.get(&key, ttl).await? {
                return Ok(b);
            }
            // Cache still empty — recursively call ourselves; the leader slot
            // is gone, so we'll either become the new leader or join the next
            // one. Box the future to keep `async fn` from inflating in size.
            return Box::pin(self.get_or_compute(key, ttl, compute)).await;
        }

        // 3. Leader double-check (a peer could have populated us while we
        //    grabbed the inflight lock — get() above doesn't hold it).
        let leader_result: Result<Bytes> = async {
            if let Some(b) = self.get(&key, ttl).await? {
                return Ok(b);
            }

            // 4. Run compute and populate both tiers.
            let bytes = compute().await?;
            self.put(&key, bytes.clone(), ttl).await?;
            Ok(bytes)
        }
        .await;

        // 5. Unregister and wake waiters, regardless of success/failure.
        {
            let mut map = self.inflight.lock().await;
            map.remove(&key);
        }
        notify.notify_waiters();

        leader_result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crs::Crs;
    use crate::encode::OutputFormat;
    use crate::tile::TileCoord;
    use opendal::{services, Operator};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn key(tag: &str) -> TileKey {
        TileKey {
            layer: tag.into(),
            coord: TileCoord::new(0, 0, 0),
            format: OutputFormat::Png,
            crs: Crs::new("EPSG:3857"),
        }
    }

    fn fresh_cache() -> LayeredCache {
        let op = Operator::new(services::Memory::default())
            .expect("memory builder")
            .finish();
        LayeredCache::from_parts(HotTier::new(1024 * 1024), PersistentTier::new(op))
    }

    #[tokio::test]
    async fn put_then_get_hits_hot_tier() {
        let cache = fresh_cache();
        let k = key("a");
        let v = Bytes::from_static(b"v");
        cache
            .put(&k, v.clone(), Duration::from_secs(60))
            .await
            .expect("put");
        let got = cache
            .get(&k, Duration::from_secs(60))
            .await
            .expect("get ok")
            .expect("present");
        assert_eq!(got, v);
    }

    #[tokio::test]
    async fn get_falls_through_to_persistent_then_promotes() {
        // Construct a cache with a fresh hot tier but a pre-populated
        // persistent tier — exercises the promotion path.
        let op = Operator::new(services::Memory::default())
            .expect("memory builder")
            .finish();
        let persistent = PersistentTier::new(op);
        let k = key("b");
        let v = Bytes::from_static(b"v");
        persistent.put(&k, v.clone()).await.expect("seed");
        let cache = LayeredCache::from_parts(HotTier::new(1024 * 1024), persistent);

        let got = cache
            .get(&k, Duration::from_secs(60))
            .await
            .expect("get ok")
            .expect("present");
        assert_eq!(got, v);
        // Second get should also succeed (now hot).
        let got2 = cache
            .get(&k, Duration::from_secs(60))
            .await
            .expect("get ok")
            .expect("present");
        assert_eq!(got2, v);
    }

    #[tokio::test]
    async fn single_flight_coalesces_concurrent_misses() {
        let cache = Arc::new(fresh_cache());
        let k = key("sf");
        let counter = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..10 {
            let cache = cache.clone();
            let k = k.clone();
            let counter = counter.clone();
            handles.push(tokio::spawn(async move {
                cache
                    .get_or_compute(k, Duration::from_secs(60), || async {
                        counter.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok(Bytes::from_static(b"v"))
                    })
                    .await
            }));
        }

        for h in handles {
            let got = h.await.expect("task ok").expect("compute ok");
            assert_eq!(got, Bytes::from_static(b"v"));
        }

        assert_eq!(
            counter.load(Ordering::SeqCst),
            1,
            "compute should have run exactly once under single-flight"
        );
    }
}
