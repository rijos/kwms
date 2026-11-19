//! The cache stack: configurable tiers (see [`CacheTier`]) with
//! single-flight coalescing.
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

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use bytesize::ByteSize;
use tokio::sync::Notify;

use super::memory::MemoryTier;
use super::store::StoreTier;
use super::{CacheTier, Entry, TileKey};
use crate::config::model::{Credentials, GlobalCacheConfig, TierConfig};

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

/// Which tiers a lookup or store uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Every tier.
    All,
    /// Only tiers that are not durable (in-process memory).
    InProcess,
}

impl Scope {
    fn includes(self, tier: &dyn CacheTier) -> bool {
        self == Scope::All || !tier.durable()
    }
}

/// The cache stack: tiers checked top to bottom (`cache.tiers` order), plus
/// single-flight coalescing of concurrent misses.
pub struct LayeredCache {
    tiers: Vec<Arc<dyn CacheTier>>,
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

/// Log and count a tier failure (the request carries on with the other
/// tiers, or renders from the source).
fn tier_error(tier: &dyn CacheTier, op: &'static str, key: &TileKey, e: &anyhow::Error) {
    metrics::counter!("kwms_cache_errors_total", "tier" => tier.name().to_string(), "op" => op).increment(1);
    tracing::warn!(
        tier = tier.name(),
        op,
        layer = %key.layer,
        path = %key.as_path(),
        error = %format_args!("{e:#}"),
        "cache tier error; serving without it"
    );
}

/// Record one tier operation's duration (`kwms_cache_seconds{tier, op}`).
fn tier_time(tier: &dyn CacheTier, op: &'static str, started: std::time::Instant) {
    metrics::histogram!("kwms_cache_seconds", "tier" => tier.name().to_string(), "op" => op)
        .record(started.elapsed().as_secs_f64());
}

impl LayeredCache {
    /// Build the stack described by `cfg.tiers`, top to bottom.
    pub fn new(cfg: &GlobalCacheConfig, credentials: &HashMap<String, Credentials>) -> Result<Self> {
        let mut tiers: Vec<Arc<dyn CacheTier>> = Vec::with_capacity(cfg.tiers.len());
        for tier in &cfg.tiers {
            let built: Arc<dyn CacheTier> = match tier {
                TierConfig::Memory(m) => {
                    let max = m
                        .max_bytes
                        .parse::<ByteSize>()
                        .map_err(|e| anyhow!("invalid max_bytes `{}`: {e}", m.max_bytes))?
                        .as_u64();
                    Arc::new(MemoryTier::new(max))
                }
                TierConfig::Store(st) => Arc::new(StoreTier::from_config(st, credentials)?),
            };
            tiers.push(built);
        }
        Ok(Self::from_tiers(tiers))
    }

    /// Assemble from already-built tiers, top to bottom — for tests and
    /// callers with custom tiers.
    pub fn from_tiers(tiers: Vec<Arc<dyn CacheTier>>) -> Self {
        Self {
            tiers,
            inflight: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// `name (durable|in-process)` per tier, top to bottom.
    pub fn describe(&self) -> Vec<String> {
        self.tiers
            .iter()
            .map(|t| format!("{} ({})", t.name(), if t.durable() { "durable" } else { "in-process" }))
            .collect()
    }

    /// Whether any tier survives a restart (what `kwms cache update` fills).
    pub fn has_durable_tier(&self) -> bool {
        self.tiers.iter().any(|t| t.durable())
    }

    /// Run every tier's startup check (a store writes, reads back and
    /// deletes a probe), so a wrong container, key or a read-only token
    /// fails the start instead of silently never caching.
    pub async fn check(&self) -> Result<()> {
        for (i, tier) in self.tiers.iter().enumerate() {
            tier.check()
                .await
                .with_context(|| format!("cache tier {i} ({})", tier.name()))?;
        }
        Ok(())
    }

    /// Look up `key`, top tier first. A hit in a lower tier is copied into
    /// the tiers above it, keeping its render time (so the TTL still counts
    /// from the render).
    ///
    /// A tier error (backend down, throttled, …) is logged, counted in
    /// `kwms_cache_errors_total{tier,op="read"}` and treated as a miss in
    /// that tier: the lookup continues below, and in the end the tile is
    /// rendered from the source rather than the request failing.
    pub async fn get(&self, key: &TileKey, ttl: Duration) -> Result<Option<Bytes>> {
        self.get_in(Scope::All, key, ttl).await
    }

    /// [`Self::get`] over the tiers in `scope` only.
    pub async fn get_in(&self, scope: Scope, key: &TileKey, ttl: Duration) -> Result<Option<Bytes>> {
        for (i, tier) in self.tiers.iter().enumerate() {
            if !scope.includes(tier.as_ref()) {
                continue;
            }
            let started = std::time::Instant::now();
            let got = tier.get(key, ttl).await;
            tier_time(tier.as_ref(), "read", started);
            match got {
                Ok(Some(entry)) => {
                    metrics::counter!("kwms_cache_hits_total", "tier" => tier.name().to_string()).increment(1);
                    for upper in self.tiers[..i].iter().filter(|t| scope.includes(t.as_ref())) {
                        if let Err(e) = upper.put(key, entry.clone()).await {
                            tier_error(upper.as_ref(), "write", key, &e);
                        }
                    }
                    return Ok(Some(entry.bytes));
                }
                Ok(None) => {}
                Err(e) => tier_error(tier.as_ref(), "read", key, &e),
            }
        }
        Ok(None)
    }

    /// Store a freshly rendered tile in every tier. A tier error is logged
    /// and counted (`op="write"`) but does not fail the request.
    pub async fn put(&self, key: &TileKey, bytes: Bytes, ttl: Duration) -> Result<()> {
        self.put_in(Scope::All, key, bytes, ttl).await
    }

    /// [`Self::put`] into the tiers in `scope` only.
    pub async fn put_in(&self, scope: Scope, key: &TileKey, bytes: Bytes, _ttl: Duration) -> Result<()> {
        let entry = Entry::now(bytes);
        for tier in self.tiers.iter().filter(|t| scope.includes(t.as_ref())) {
            let started = std::time::Instant::now();
            let put = tier.put(key, entry.clone()).await;
            tier_time(tier.as_ref(), "write", started);
            if let Err(e) = put {
                tier_error(tier.as_ref(), "write", key, &e);
            }
        }
        Ok(())
    }

    /// For `kwms cache update`: whether every durable tier holds a fresh
    /// copy of `key`. A copy found in some durable tiers is written to the
    /// durable tiers missing it (keeping its render time), so the tile need
    /// not be rendered again. Errors are returned, not degraded: an update
    /// must not report tiles as cached when a backend failed.
    pub async fn durable_fresh(&self, key: &TileKey, ttl: Duration) -> Result<bool> {
        let mut found: Option<Entry> = None;
        let mut missing = Vec::new();
        for tier in self.tiers.iter().filter(|t| t.durable()) {
            match tier
                .get(key, ttl)
                .await
                .with_context(|| format!("cache tier {}: read", tier.name()))?
            {
                Some(entry) => found = found.or(Some(entry)),
                None => missing.push(tier),
            }
        }
        let Some(entry) = found else { return Ok(false) };
        for tier in missing {
            tier.put(key, entry.clone())
                .await
                .with_context(|| format!("cache tier {}: write", tier.name()))?;
        }
        Ok(true)
    }

    /// For `kwms cache update`: write a freshly rendered tile to every
    /// durable tier, errors returned (see [`Self::durable_fresh`]).
    pub async fn store_durable(&self, key: &TileKey, bytes: Bytes) -> Result<()> {
        let entry = Entry::now(bytes);
        for tier in self.tiers.iter().filter(|t| t.durable()) {
            tier.put(key, entry.clone())
                .await
                .with_context(|| format!("cache tier {}: write", tier.name()))?;
        }
        Ok(())
    }

    /// Get-or-compute with per-key single-flight. See the module docs for the
    /// algorithm and its concurrency properties.
    pub async fn get_or_compute<F, Fut>(&self, key: TileKey, ttl: Duration, compute: F) -> Result<Bytes>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Computed>>,
    {
        self.get_or_compute_in(Scope::All, key, ttl, compute).await
    }

    /// [`Self::get_or_compute`] over the tiers in `scope` only.
    pub async fn get_or_compute_in<F, Fut>(
        &self,
        scope: Scope,
        key: TileKey,
        ttl: Duration,
        compute: F,
    ) -> Result<Bytes>
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = Result<Computed>>,
    {
        let mut compute = Some(compute);
        loop {
            // 1. Fast path.
            if let Some(b) = self.get_in(scope, &key, ttl).await? {
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
                        let entry = Arc::new(Inflight {
                            notify: Arc::new(Notify::new()),
                            result: OnceLock::new(),
                        });
                        map.insert(key.clone(), entry.clone());
                        Ok(InflightGuard {
                            inflight: self.inflight.clone(),
                            key: key.clone(),
                            entry,
                        })
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
            if let Some(b) = self.get_in(scope, &key, ttl).await? {
                return Ok(b);
            }
            // A real miss: counted once, by the leader that renders it.
            metrics::counter!("kwms_cache_misses_total").increment(1);
            let compute = compute.take().expect("compute is used at most once: a leader returns");
            let Computed { bytes, store } = compute().await?;
            if store {
                self.put_in(scope, &key, bytes.clone(), ttl).await?;
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
    use crate::cache::memory::MemoryTier;
    use crate::cache::store::StoreTier;
    use crate::encode::OutputFormat;
    use crate::tile::TileCoord;
    use opendal::{services, Operator};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn key(tag: &str) -> TileKey {
        TileKey {
            layer: tag.into(),
            coord: TileCoord::new(0, 0, 0),
            format: OutputFormat::Png,
            grid_dir: "3857".into(),
        }
    }

    fn fresh_cache() -> LayeredCache {
        LayeredCache::from_tiers(vec![Arc::new(MemoryTier::new(1024 * 1024))])
    }

    fn fs_tier(dir: &std::path::Path) -> Arc<StoreTier> {
        Arc::new(StoreTier::new(
            Operator::new(services::Fs::default().root(&dir.to_string_lossy()))
                .unwrap()
                .finish(),
        ))
    }

    #[tokio::test]
    async fn put_then_get_hits_memory_tier() {
        let cache = fresh_cache();
        let k = key("a");
        let v = Bytes::from_static(b"v");
        cache.put(&k, v.clone(), Duration::from_secs(60)).await.expect("put");
        let got = cache
            .get(&k, Duration::from_secs(60))
            .await
            .expect("get ok")
            .expect("present");
        assert_eq!(got, v);
    }

    /// memory → disk → disk: a hit in the bottom tier is copied into both
    /// tiers above it with its original render time; a new tile is written
    /// to all three.
    #[tokio::test]
    async fn three_tier_stack_promotes_and_writes_through() {
        let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (mem, mid, bottom) = (
            Arc::new(MemoryTier::new(1 << 20)),
            fs_tier(d1.path()),
            fs_tier(d2.path()),
        );
        let cache = LayeredCache::from_tiers(vec![mem.clone(), mid.clone(), bottom.clone()]);
        let ttl = Duration::from_secs(60);

        let k = key("deep");
        let old = Entry {
            bytes: Bytes::from_static(b"v"),
            written_at: Entry::now(Bytes::new()).written_at - 30,
        };
        bottom.put(&k, old.clone()).await.unwrap();
        assert_eq!(cache.get(&k, ttl).await.unwrap(), Some(Bytes::from_static(b"v")));
        assert_eq!(
            mem.get(&k, ttl).await.unwrap(),
            Some(old.clone()),
            "promoted with its render time"
        );
        assert_eq!(mid.get(&k, ttl).await.unwrap(), Some(old.clone()));
        // The copies expire when the original would.
        assert!(mid.get(&k, Duration::from_secs(20)).await.unwrap().is_none());

        let n = key("new");
        cache.put(&n, Bytes::from_static(b"n"), ttl).await.unwrap();
        for tier in [mem.clone() as Arc<dyn CacheTier>, mid.clone(), bottom.clone()] {
            assert!(
                tier.get(&n, ttl).await.unwrap().is_some(),
                "{} missing the tile",
                tier.name()
            );
        }
        assert_eq!(
            cache.describe(),
            ["memory (in-process)", "fs (durable)", "fs (durable)"]
        );
    }

    /// Deeper than `cache.max_level`: tiles live in memory only — never
    /// read from or written to a durable tier.
    #[tokio::test]
    async fn in_process_scope_skips_durable_tiers() {
        let dir = tempfile::tempdir().unwrap();
        let disk = fs_tier(dir.path());
        let cache = LayeredCache::from_tiers(vec![Arc::new(MemoryTier::new(1 << 20)), disk.clone()]);
        let ttl = Duration::from_secs(60);
        let got = cache
            .get_or_compute_in(Scope::InProcess, key("deep"), ttl, || async {
                Ok(Computed::store(Bytes::from_static(b"d")))
            })
            .await
            .unwrap();
        assert_eq!(got, Bytes::from_static(b"d"));
        assert!(
            disk.get(&key("deep"), ttl).await.unwrap().is_none(),
            "written to the durable tier"
        );
        assert_eq!(
            cache.get_in(Scope::InProcess, &key("deep"), ttl).await.unwrap(),
            Some(Bytes::from_static(b"d"))
        );
        // A tile only on disk is invisible to an in-process lookup.
        disk.put(&key("disk"), Entry::now(Bytes::from_static(b"x")))
            .await
            .unwrap();
        assert!(cache
            .get_in(Scope::InProcess, &key("disk"), ttl)
            .await
            .unwrap()
            .is_none());
    }

    /// `cache update` helpers: a copy in one durable tier fills the other;
    /// with none, the tile is stale.
    #[tokio::test]
    async fn durable_fresh_fills_missing_durable_tiers() {
        let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (mid, bottom) = (fs_tier(d1.path()), fs_tier(d2.path()));
        let cache = LayeredCache::from_tiers(vec![Arc::new(MemoryTier::new(1 << 20)), mid.clone(), bottom.clone()]);
        let ttl = Duration::from_secs(60);
        assert!(!cache.durable_fresh(&key("x"), ttl).await.unwrap());
        bottom
            .put(&key("x"), Entry::now(Bytes::from_static(b"x")))
            .await
            .unwrap();
        assert!(cache.durable_fresh(&key("x"), ttl).await.unwrap());
        assert!(mid.get(&key("x"), ttl).await.unwrap().is_some());
        cache.store_durable(&key("y"), Bytes::from_static(b"y")).await.unwrap();
        assert!(
            mid.get(&key("y"), ttl).await.unwrap().is_some() && bottom.get(&key("y"), ttl).await.unwrap().is_some()
        );
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
        let cache = Arc::new(fresh_cache());
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
            c.get_or_compute(k, ttl, || async { Ok(Computed::store(Bytes::from_static(b"fresh"))) })
                .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        leader.abort();

        let out = tokio::time::timeout(Duration::from_secs(2), follower)
            .await
            .expect("follower stranded after the leader was cancelled");
        assert_eq!(out.unwrap().unwrap(), Bytes::from_static(b"fresh"));
        let again = tokio::time::timeout(
            Duration::from_secs(2),
            cache.get_or_compute(key("cancel"), ttl, || async {
                Ok(Computed::store(Bytes::from_static(b"other")))
            }),
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
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "round {round}: computed more than once"
            );
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
        let follower = cache.get_or_compute(key("fail"), ttl, || async {
            Ok(Computed::store(Bytes::from_static(b"ok")))
        });
        let out = tokio::time::timeout(Duration::from_secs(2), follower)
            .await
            .expect("follower hung");
        assert_eq!(out.unwrap(), Bytes::from_static(b"ok"));
        assert!(leader.await.unwrap().is_err());
    }

    /// A failing store tier (outage, throttling, lost permissions)
    /// must not fail requests: the tile is rendered and served anyway.
    #[cfg(unix)]
    #[tokio::test]
    async fn failing_store_tier_degrades_to_rendering() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let disk = fs_tier(dir.path());
        disk.check().await.expect("writable at start");
        let cache = LayeredCache::from_tiers(vec![Arc::new(MemoryTier::new(1024 * 1024)), disk]);

        // The backend breaks while serving.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        if std::fs::write(dir.path().join("probe"), b"x").is_ok() {
            return; // running as root: permissions aren't enforced, nothing to test
        }
        let ttl = Duration::from_secs(60);
        let got = cache
            .get_or_compute(key("broken"), ttl, || async {
                Ok(Computed::store(Bytes::from_static(b"tile")))
            })
            .await
            .expect("request must not fail because a cache tier does");
        assert_eq!(got, Bytes::from_static(b"tile"));
        // Still served from the memory tier afterwards.
        assert_eq!(
            cache.get(&key("broken"), ttl).await.unwrap(),
            Some(Bytes::from_static(b"tile"))
        );
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
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
            async move {
                cache
                    .get_or_compute(key("transient"), ttl, compute(computed, release))
                    .await
            }
        });
        // Wait until the leader is computing, then queue followers behind it.
        while computed.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        let followers: Vec<_> = (0..16)
            .map(|_| {
                let (cache, computed, release) = (cache.clone(), computed.clone(), release.clone());
                tokio::spawn(async move {
                    cache
                        .get_or_compute(key("transient"), ttl, compute(computed, release))
                        .await
                })
            })
            .collect();
        // All 16 followers hold the entry (map + leader + 16) — each created
        // its wait future under the lock, so none can miss the wake-up.
        let holders = || {
            lock_inflight(&cache.inflight)
                .get(&key("transient"))
                .map(Arc::strong_count)
        };
        while holders() != Some(18) {
            tokio::task::yield_now().await;
        }
        release.notify_waiters();
        assert_eq!(leader.await.unwrap().unwrap(), Bytes::from_static(b"empty"));
        for f in followers {
            let out = tokio::time::timeout(Duration::from_secs(5), f)
                .await
                .expect("follower hung");
            assert_eq!(out.unwrap().unwrap(), Bytes::from_static(b"empty"));
        }
        assert_eq!(
            computed.load(Ordering::SeqCst),
            1,
            "one computation for the whole burst"
        );
        assert!(
            cache.get(&key("transient"), ttl).await.unwrap().is_none(),
            "transient result was stored"
        );
    }
}
