//! Persistent tier — wraps an [`opendal::Operator`] with a TTL check.
//!
//! See `docs/src/caching.md` (Storage stack, Invalidation).
//!
//! ## TTL strategy
//!
//! On `put`, the data is written at `<path>` and a small JSON sidecar is
//! written at `<path>.meta` containing the unix-seconds write timestamp:
//! `{"written_at": <u64>}`. On `get`, the sidecar is read and compared
//! against the supplied TTL — if expired, the entry is treated as a miss
//! and the data + sidecar are deleted on a best-effort basis.
//!
//! We use a sidecar rather than `Metadata::last_modified` because not every
//! OpenDAL backend reports `last_modified` (in particular the `Memory`
//! service used in tests). The sidecar is portable across all backends.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use bytes::Bytes;
use opendal::{ErrorKind, Operator};
use serde::{Deserialize, Serialize};

use super::TileKey;
use crate::config::model::{Credentials, PersistentCacheConfig};

/// Persistent cache tier backed by an OpenDAL operator.
pub struct PersistentTier {
    op: Operator,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct MetaSidecar {
    written_at: u64,
}

fn meta_path(path: &str) -> String {
    format!("{path}.meta")
}

fn now_unix_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl PersistentTier {
    pub fn new(op: Operator) -> Self {
        Self { op }
    }

    /// Fetch the entry. Returns `Ok(None)` when:
    ///   * the data object does not exist, or
    ///   * the sidecar metadata is missing/unparseable, or
    ///   * the sidecar's `written_at` is older than `ttl`.
    pub async fn get(&self, key: &TileKey, ttl: Duration) -> Result<Option<Bytes>> {
        let path = key.as_path();
        let meta_path = meta_path(&path);

        // 1) Read the sidecar to decide freshness.
        let meta_buf = match self.op.read(&meta_path).await {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(anyhow!(e).context("persistent cache: read sidecar")),
        };
        let meta_bytes = meta_buf.to_bytes();
        let meta: MetaSidecar = match serde_json::from_slice(&meta_bytes) {
            Ok(m) => m,
            Err(_) => return Ok(None),
        };
        let age = now_unix_seconds().saturating_sub(meta.written_at);
        if age > ttl.as_secs() {
            // Expired — best-effort cleanup, then miss.
            let _ = self.op.delete(&path).await;
            let _ = self.op.delete(&meta_path).await;
            return Ok(None);
        }

        // 2) Fresh — read the data.
        let data = match self.op.read(&path).await {
            Ok(b) => b.to_bytes(),
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(anyhow!(e).context("persistent cache: read data")),
        };
        Ok(Some(data))
    }

    /// Store the entry. Writes the data and then a fresh `.meta` sidecar
    /// recording the current unix timestamp.
    pub async fn put(&self, key: &TileKey, bytes: Bytes) -> Result<()> {
        let path = key.as_path();
        self.op
            .write(&path, bytes)
            .await
            .map_err(|e| anyhow!(e).context("persistent cache: write data"))?;
        let sidecar = MetaSidecar {
            written_at: now_unix_seconds(),
        };
        let sidecar_bytes = serde_json::to_vec(&sidecar)
            .map_err(|e| anyhow!(e).context("persistent cache: serialize sidecar"))?;
        self.op
            .write(&meta_path(&path), sidecar_bytes)
            .await
            .map_err(|e| anyhow!(e).context("persistent cache: write sidecar"))?;
        Ok(())
    }
}

/// Build an [`Operator`] from a [`PersistentCacheConfig`].
///
/// Only the `Memory` backend is fully wired here. The other backends are
/// gated behind opendal Cargo features that are not currently enabled
/// (`services-fs`, `services-s3`, `services-azblob`, `services-redis`,
/// `services-memcached`); they return an error explaining how to enable
/// them. This is the "phase 2.x" stub flagged in the cache-agent brief.
pub fn build_operator(
    cfg: &PersistentCacheConfig,
    _credentials: &HashMap<String, Credentials>,
) -> Result<Operator> {
    match cfg {
        PersistentCacheConfig::Fs { path } => {
            // PHASE 2.x STUB: enable `opendal` feature `services-fs` in
            // Cargo.toml and wire `opendal::services::Fs::default().root(...)`.
            Err(anyhow!(
                "persistent cache backend `fs` (path={}) requires the `opendal` \
                 `services-fs` feature, which is not enabled in Cargo.toml \
                 (phase 2.x)",
                path.display()
            ))
        }
        PersistentCacheConfig::Azure { container, .. } => Err(anyhow!(
            "persistent cache backend `azure` (container={container}) requires \
             the `opendal` `services-azblob` feature (phase 2.x)"
        )),
        PersistentCacheConfig::S3 { bucket, .. } => Err(anyhow!(
            "persistent cache backend `s3` (bucket={bucket}) requires the \
             `opendal` `services-s3` feature (phase 2.x)"
        )),
        PersistentCacheConfig::Redis { url } => Err(anyhow!(
            "persistent cache backend `redis` (url={url}) requires the \
             `opendal` `services-redis` feature (phase 2.x)"
        )),
        PersistentCacheConfig::Memcached { url } => Err(anyhow!(
            "persistent cache backend `memcached` (url={url}) requires the \
             `opendal` `services-memcached` feature (phase 2.x)"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crs::Crs;
    use crate::encode::OutputFormat;
    use crate::tile::TileCoord;
    use opendal::services;

    fn memory_operator() -> Operator {
        Operator::new(services::Memory::default())
            .expect("memory builder")
            .finish()
    }

    fn key() -> TileKey {
        TileKey {
            layer: "L".into(),
            coord: TileCoord::new(0, 0, 0),
            format: OutputFormat::Png,
            crs: Crs::new("EPSG:3857"),
        }
    }

    #[tokio::test]
    async fn put_then_get_returns_bytes_within_ttl() {
        let tier = PersistentTier::new(memory_operator());
        let v = Bytes::from_static(b"abc");
        tier.put(&key(), v.clone()).await.expect("put");
        let got = tier
            .get(&key(), Duration::from_secs(60))
            .await
            .expect("get ok")
            .expect("present");
        assert_eq!(got, v);
    }

    #[tokio::test]
    async fn get_after_ttl_returns_none() {
        let tier = PersistentTier::new(memory_operator());
        let v = Bytes::from_static(b"abc");
        tier.put(&key(), v.clone()).await.expect("put");
        // Sleep 2s, then ask with a 1s TTL — should be a miss.
        tokio::time::sleep(Duration::from_secs(2)).await;
        let got = tier
            .get(&key(), Duration::from_secs(1))
            .await
            .expect("get ok");
        assert!(got.is_none(), "expected expired entry to miss");
    }

    #[tokio::test]
    async fn missing_key_returns_none() {
        let tier = PersistentTier::new(memory_operator());
        let got = tier
            .get(&key(), Duration::from_secs(60))
            .await
            .expect("get ok");
        assert!(got.is_none());
    }

    #[test]
    fn build_operator_fs_is_stubbed_with_clear_error() {
        let cfg = PersistentCacheConfig::default();
        let err = build_operator(&cfg, &HashMap::new()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("services-fs"), "got: {msg}");
    }
}
