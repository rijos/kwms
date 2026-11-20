//! The interface every cache tier implements, so tiers can be stacked in
//! any order (`cache.tiers` in `config.yaml`).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use bytes::Bytes;

use super::TileKey;

/// A cached tile and when it was rendered (unix seconds).
///
/// The write time travels with the tile when [`super::LayeredCache`] copies
/// it into a higher tier, so the layer's TTL counts from the render in every
/// tier: copying never makes a tile younger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub bytes: Bytes,
    pub written_at: u64,
    /// Fingerprint of the layer's render settings when the tile was made
    /// (`pipeline::fingerprint`). `kwms cache update` re-renders tiles whose
    /// fingerprint differs; serving ignores it.
    pub render: Option<String>,
}

impl Entry {
    /// A tile rendered now.
    pub fn now(bytes: Bytes) -> Self {
        Self {
            bytes,
            written_at: now_unix_seconds(),
            render: None,
        }
    }

    /// Whether the entry must not be served (see [`stale`]).
    pub fn expired(&self, ttl: Duration) -> bool {
        stale(self.written_at, ttl)
    }
}

/// Clock difference tolerated between hosts writing a shared tier.
pub(crate) const MAX_CLOCK_SKEW_SECS: u64 = 60;

/// Whether a tile rendered at `written_at` (Unix seconds, UTC) must not be
/// served: older than `ttl`, or claiming to be from the future. A timestamp
/// past now (plus clock skew) is not trusted — it would never expire.
pub(crate) fn stale(written_at: u64, ttl: Duration) -> bool {
    let now = now_unix_seconds();
    written_at > now.saturating_add(MAX_CLOCK_SKEW_SECS) || now.saturating_sub(written_at) > ttl.as_secs()
}

pub(crate) fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// One level of the cache stack.
///
/// Implementations must be safe to call concurrently. Errors mean "this
/// tier is unusable right now"; the stack logs and counts them and carries
/// on with the other tiers (see [`super::LayeredCache`]), so a tier never
/// has to hide its own failures.
#[async_trait::async_trait]
pub trait CacheTier: Send + Sync {
    /// Short label for logs and metrics (`memory`, `fs`, `azblob`, …).
    fn name(&self) -> &str;

    /// Whether the tier survives a restart and is shared with other
    /// processes — what `kwms cache update` fills.
    fn durable(&self) -> bool;

    /// The entry for `key`, if present and not older than `ttl`. An expired
    /// entry is a miss (the tier may delete it).
    async fn get(&self, key: &TileKey, ttl: Duration) -> Result<Option<Entry>>;

    /// Store `entry` under `key`, keeping its `written_at`.
    async fn put(&self, key: &TileKey, entry: Entry) -> Result<()>;

    /// Startup check that the tier is usable. Default: nothing to check.
    async fn check(&self) -> Result<()> {
        Ok(())
    }
}
