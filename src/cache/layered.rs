//! Two-tier cache (hot + persistent) with single-flight coalescing.
//!
//! See `docs/src/caching.md` (Stampede protection).
//!
//! # Single-flight algorithm
//!
//! For each `key`, at most one `compute` runs at a time across this process.
//!
//! ```text
//! get_or_compute(key, ttl, compute):
//!   loop:
//!     1. Fast path: if `get(key, ttl)` hits, return it.
//!     2. Under the in-flight map lock (a std Mutex, never held across an
//!        await):
//!        - a leader is registered for `key` → create our `Notified` future
//!          *while still holding the lock*, release it, wait, and loop;
//!        - otherwise register ourselves as leader with an `InflightGuard`.
//!     3. Leader double-checks the cache, runs `compute`, stores the result
//!        unless `compute` marked it transient, and publishes it in the
//!        in-flight entry.
//!     4. The guard's `Drop` unregisters the key and wakes all followers —
//!        on success, on error, *and when the leader's future is dropped*
//!        (client gone, request timeout). A follower that wakes to a
//!        published result returns it; otherwise it loops.
//! ```
//!
//! Concurrency properties (see `docs/src/concurrency.md`):
//! * **No lost wake-up:** tokio guarantees a `Notified` receives every
//!   `notify_waiters()` issued after it was *created*; followers create it
//!   under the same lock the leader takes to unregister, so a wake-up can't
//!   slip between "saw a leader" and "started waiting".
//! * **No stranded key:** unregistering happens in `Drop`, which also runs
//!   on cancellation, so a cancelled leader can't block a tile forever.
//! * **No deadlock:** the map lock is held only for a `HashMap` operation and
//!   never while awaiting or calling anything else.
//! * If a leader fails, followers re-run the loop and one becomes the next
//!   leader (errors are not shared between peers).
//! * **Transient results** (e.g. all-nodata tiles, which are not cached so
//!   clients can't fill the cache with them) still reach every follower
//!   that was waiting, so they are computed once per burst, not once per
//!   waiter.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use anyhow::{anyhow, Result};
use bytes::Bytes;
use bytesize::ByteSize;
use tokio::sync::Notify;

use super::hot::HotTier;
use super::persistent::{build_operator, PersistentTier};
use super::TileKey;
use crate::config::model::{Credentials, GlobalCacheConfig};

/// What a `get_or_compute` computation produced.
pub struct Computed {
    pub bytes: Bytes,
    /// Store in the cache tiers (`false`: hand to waiting peers only).
    pub store: bool,
}

impl Computed {
    /// A result to cache.
    pub fn store(bytes: Bytes) -> Self {
        Self { bytes, store: true }
    }
    /// A result to return (also to waiting peers) but not cache.
    pub fn transient(bytes: Bytes) -> Self {
        Self { bytes, store: false }
    }
}

/// One in-flight computation: followers wait on `notify`; the leader
/// publishes its successful result in `result` before waking them.
struct Inflight {
    notify: Arc<Notify>,
    result: OnceLock<Bytes>,
}

type InflightMap = HashMap<TileKey, Arc<Inflight>>;

pub struct LayeredCache {
    hot: HotTier,
    persistent: PersistentTier,
    inflight: Arc<Mutex<InflightMap>>,
}

/// Lock the in-flight map. Poisoning is tolerated: the map is only ever
/// mutated by single `insert`/`remove` calls, so it is always consistent.
fn lock_inflight(m: &Mutex<InflightMap>) -> MutexGuard<'_, InflightMap> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Held by the single-flight leader. Dropping it — normally, on error, or
/// because the leader's future was cancelled — unregisters the key and wakes
/// every follower.
struct InflightGuard {
    inflight: Arc<Mutex<InflightMap>>,
    key: TileKey,
    entry: Arc<Inflight>,
}

impl Drop for InflightGuard {
    fn drop(&mut self) {
        {
            let mut map = lock_inflight(&self.inflight);
            // Remove only our own registration.
            if map.get(&self.key).is_some_and(|e| Arc::ptr_eq(e, &self.entry)) {
                map.remove(&self.key);
            }
        }
        self.entry.notify.notify_waiters();
    }
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
            inflight: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    /// Construct from already-built tiers — useful for tests and callers
    /// that have a custom `Operator` (e.g. wrapped in layers).
    pub fn from_parts(hot: HotTier, persistent: PersistentTier) -> Self {
        Self {
            hot,
            persistent,
            inflight: Arc::new(Mutex::new(HashMap::new())),
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
    /// algorithm and its concurrency properties.
    pub async fn get_or_compute<F, Fut>(&self, key: TileKey, ttl: Duration, compute: F) -> Result<Bytes>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Computed>>,
    {
        let mut compute = Some(compute);
        loop {
            // 1. Fast path.
            if let Some(b) = self.get(&key, ttl).await? {
                return Ok(b);
            }

            // 2. Join the current leader, or become the leader.
            let role = {
                let mut map = lock_inflight(&self.inflight);
                match map.get(&key) {
                    // Created under the lock: guaranteed to see the leader's
                    // notify_waiters(), which happens after it removes the key.
                    Some(existing) => Err((existing.clone(), existing.notify.clone().notified_owned())),
                    None => {
                        let entry = Arc::new(Inflight { notify: Arc::new(Notify::new()), result: OnceLock::new() });
                        map.insert(key.clone(), entry.clone());
                        Ok(InflightGuard { inflight: self.inflight.clone(), key: key.clone(), entry })
                    }
                }
            };
            let guard = match role {
                Ok(guard) => guard,
                Err((entry, wait)) => {
                    wait.await;
                    // The leader published before waking us: take its result.
                    if let Some(bytes) = entry.result.get() {
                        return Ok(bytes.clone());
                    }
                    continue;
                }
            };

            // 3. Leader: double-check (a peer may have stored it meanwhile),
            //    then compute and store. `guard` unregisters on every exit.
            if let Some(b) = self.get(&key, ttl).await? {
                return Ok(b);
            }
            let compute = compute.take().expect("compute is used at most once: a leader returns");
            let Computed { bytes, store } = compute().await?;
            if store {
                self.put(&key, bytes.clone(), ttl).await?;
            }
            // Set at most once: this guard's entry has a single leader.
            let _ = guard.entry.result.set(bytes.clone());
            return Ok(bytes);
        }
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
                        Ok(Computed::store(Bytes::from_static(b"v")))
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

    /// A leader whose request is cancelled (client gone, request timeout)
    /// must not leave its key registered: followers and later requests for
    /// the same tile would otherwise wait forever.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_leader_does_not_strand_followers() {
        let op = Operator::new(services::Memory::default()).unwrap().finish();
        let cache = Arc::new(LayeredCache::from_parts(HotTier::new(1024 * 1024), PersistentTier::new(op)));
        let ttl = Duration::from_secs(60);

        let (c, k) = (cache.clone(), key("cancel"));
        let leader = tokio::spawn(async move {
            c.get_or_compute(k, ttl, || async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(Computed::store(Bytes::from_static(b"never")))
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let (c, k) = (cache.clone(), key("cancel"));
        let follower = tokio::spawn(async move {
            c.get_or_compute(k, ttl, || async { Ok(Computed::store(Bytes::from_static(b"fresh"))) }).await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        leader.abort();

        let out = tokio::time::timeout(Duration::from_secs(2), follower)
            .await
            .expect("follower stranded after the leader was cancelled");
        assert_eq!(out.unwrap().unwrap(), Bytes::from_static(b"fresh"));
        let again = tokio::time::timeout(
            Duration::from_secs(2),
            cache.get_or_compute(key("cancel"), ttl, || async { Ok(Computed::store(Bytes::from_static(b"other"))) }),
        )
        .await
        .expect("key stuck in the in-flight map");
        assert!(again.is_ok());
    }

    /// Many rounds of many concurrent misses on a fresh key: every caller
    /// must finish (a lost wake-up would hang a follower) and each round
    /// must compute exactly once.
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn single_flight_stress_never_hangs_and_computes_once() {
        let cache = Arc::new(fresh_cache());
        let ttl = Duration::from_secs(60);
        for round in 0..300 {
            let calls = Arc::new(AtomicUsize::new(0));
            let mut tasks = Vec::new();
            for _ in 0..32 {
                let (cache, calls) = (cache.clone(), calls.clone());
                tasks.push(tokio::spawn(async move {
                    cache
                        .get_or_compute(key(&format!("stress-{round}")), ttl, || async move {
                            calls.fetch_add(1, Ordering::SeqCst);
                            tokio::task::yield_now().await;
                            Ok(Computed::store(Bytes::from_static(b"v")))
                        })
                        .await
                }));
            }
            for t in tasks {
                let r = tokio::time::timeout(Duration::from_secs(5), t)
                    .await
                    .unwrap_or_else(|_| panic!("round {round}: a caller hung"));
                assert_eq!(r.unwrap().unwrap(), Bytes::from_static(b"v"));
            }
            assert_eq!(calls.load(Ordering::SeqCst), 1, "round {round}: computed more than once");
        }
    }

    /// When the leader fails, a waiting follower becomes the next leader.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failed_leader_hands_over_to_a_follower() {
        let cache = Arc::new(fresh_cache());
        let ttl = Duration::from_secs(60);
        let (c, k) = (cache.clone(), key("fail"));
        let leader = tokio::spawn(async move {
            c.get_or_compute(k, ttl, || async {
                tokio::time::sleep(Duration::from_millis(100)).await;
                Err(anyhow!("source down"))
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        let follower = cache.get_or_compute(key("fail"), ttl, || async { Ok(Computed::store(Bytes::from_static(b"ok"))) });
        let out = tokio::time::timeout(Duration::from_secs(2), follower).await.expect("follower hung");
        assert_eq!(out.unwrap(), Bytes::from_static(b"ok"));
        assert!(leader.await.unwrap().is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn transient_results_reach_waiters_but_are_not_stored() {
        let cache = Arc::new(fresh_cache());
        let ttl = Duration::from_secs(60);
        let computed = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(Notify::new());
        let compute = |computed: Arc<AtomicUsize>, release: Arc<Notify>| {
            move || async move {
                computed.fetch_add(1, Ordering::SeqCst);
                release.notified().await;
                Ok(Computed::transient(Bytes::from_static(b"empty")))
            }
        };
        let leader = tokio::spawn({
            let (cache, computed, release) = (cache.clone(), computed.clone(), release.clone());
            async move { cache.get_or_compute(key("transient"), ttl, compute(computed, release)).await }
        });
        // Wait until the leader is computing, then queue followers behind it.
        while computed.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        let followers: Vec<_> = (0..16)
            .map(|_| {
                let (cache, computed, release) = (cache.clone(), computed.clone(), release.clone());
                tokio::spawn(async move { cache.get_or_compute(key("transient"), ttl, compute(computed, release)).await })
            })
            .collect();
        // All 16 followers hold the entry (map + leader + 16) — each created
        // its wait future under the lock, so none can miss the wake-up.
        let holders = || lock_inflight(&cache.inflight).get(&key("transient")).map(Arc::strong_count);
        while holders() != Some(18) {
            tokio::task::yield_now().await;
        }
        release.notify_waiters();
        assert_eq!(leader.await.unwrap().unwrap(), Bytes::from_static(b"empty"));
        for f in followers {
            let out = tokio::time::timeout(Duration::from_secs(5), f).await.expect("follower hung");
            assert_eq!(out.unwrap().unwrap(), Bytes::from_static(b"empty"));
        }
        assert_eq!(computed.load(Ordering::SeqCst), 1, "one computation for the whole burst");
        assert!(cache.get(&key("transient"), ttl).await.unwrap().is_none(), "transient result was stored");
    }
}
