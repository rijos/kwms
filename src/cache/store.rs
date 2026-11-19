//! Storage tier — any [`opendal`] backend (`fs`, `azblob`, …) as a
//! [`CacheTier`].
//!
//! See `docs/src/caching.md` (Storage stack, Invalidation).
//!
//! ## TTL strategy
//!
//! Each tile is stored at `<path>` with a small JSON sidecar at
//! `<path>.meta` holding its render time: `{"written_at": <unix secs>}`. On
//! `get`, an entry older than the TTL is a miss and is deleted on a
//! best-effort basis. A sidecar rather than `Metadata::last_modified`
//! because not every backend reports `last_modified`, and because a tile
//! copied in from another tier keeps its original render time.

use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use opendal::{ErrorKind, Operator, Scheme};
use serde::{Deserialize, Serialize};

use super::tier::now_unix_seconds;
use super::{CacheTier, Entry, TileKey};
use crate::config::model::{Credentials, StoreTierConfig};

/// Cache tier backed by an opendal operator.
pub struct StoreTier {
    op: Operator,
    name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct MetaSidecar {
    written_at: u64,
}

fn meta_path(path: &str) -> String {
    format!("{path}.meta")
}

/// Path of the startup probe. Starts with a `.`, which no layer name may
/// (`config::validate`), so it never collides with a tile.
const PROBE: &str = ".kwms-startup-check";

impl StoreTier {
    pub fn new(op: Operator) -> Self {
        let name = op.info().scheme().to_string();
        Self { op, name }
    }

    /// Build the operator from config (see [`build_operator`]).
    pub fn from_config(cfg: &StoreTierConfig, credentials: &HashMap<String, Credentials>) -> Result<Self> {
        Ok(Self::new(build_operator(cfg, credentials)?))
    }
}

#[async_trait::async_trait]
impl CacheTier for StoreTier {
    fn name(&self) -> &str {
        &self.name
    }

    /// Everything except opendal's in-memory service.
    fn durable(&self) -> bool {
        self.op.info().scheme() != Scheme::Memory
    }

    async fn get(&self, key: &TileKey, ttl: Duration) -> Result<Option<Entry>> {
        let path = key.as_path();
        let meta_path = meta_path(&path);

        // 1) The sidecar decides freshness.
        let meta_buf = match self.op.read(&meta_path).await {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(anyhow!(e).context("read sidecar")),
        };
        let Ok(meta) = serde_json::from_slice::<MetaSidecar>(&meta_buf.to_bytes()) else {
            return Ok(None);
        };
        if now_unix_seconds().saturating_sub(meta.written_at) > ttl.as_secs() {
            // Expired — best-effort cleanup, then miss.
            let _ = self.op.delete(&path).await;
            let _ = self.op.delete(&meta_path).await;
            return Ok(None);
        }

        // 2) Fresh — read the data.
        match self.op.read(&path).await {
            Ok(b) => Ok(Some(Entry {
                bytes: b.to_bytes(),
                written_at: meta.written_at,
            })),
            Err(e) if e.kind() == ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow!(e).context("read data")),
        }
    }

    /// Data first, then the sidecar: a reader never sees a sidecar without
    /// its data (a crash in between leaves data without sidecar = a miss).
    async fn put(&self, key: &TileKey, entry: Entry) -> Result<()> {
        let path = key.as_path();
        self.op
            .write(&path, entry.bytes)
            .await
            .map_err(|e| anyhow!(e).context("write data"))?;
        let sidecar = serde_json::to_vec(&MetaSidecar {
            written_at: entry.written_at,
        })
        .context("serialize sidecar")?;
        self.op
            .write(&meta_path(&path), sidecar)
            .await
            .map_err(|e| anyhow!(e).context("write sidecar"))?;
        Ok(())
    }

    /// Write, read back and delete a probe object.
    async fn check(&self) -> Result<()> {
        let body = format!("kwms {}", now_unix_seconds());
        self.op.write(PROBE, body.clone()).await.map_err(|e| {
            anyhow!(e).context(
                "cannot write (check backend, container/root and credentials; the token must allow write and delete)",
            )
        })?;
        let back = self
            .op
            .read(PROBE)
            .await
            .map_err(|e| anyhow!(e).context("cannot read back what was written"))?;
        if back.to_bytes() != body.as_bytes() {
            return Err(anyhow!("read back different bytes than were written"));
        }
        self.op
            .delete(PROBE)
            .await
            .map_err(|e| anyhow!(e).context("cannot delete (expired tiles could never be removed)"))?;
        Ok(())
    }
}

/// Build an [`Operator`] from a [`StoreTierConfig`].
///
/// Two delegations to opendal:
///
/// 1. The backend name is parsed via [`Scheme::from_str`]
///    (<https://docs.rs/opendal/0.50/opendal/enum.Scheme.html#impl-FromStr-for-Scheme>).
/// 2. The merged option map is handed to [`Operator::via_iter`]
///    (<https://docs.rs/opendal/0.50/opendal/struct.Operator.html#method.via_iter>),
///    which dispatches to the matching builder in
///    <https://docs.rs/opendal/0.50/opendal/services/index.html>.
///
/// A backend whose opendal Cargo feature is not compiled in fails with
/// opendal's `Unsupported` error, propagated verbatim.
///
/// Credentials: when `cfg.credentials` names a block in `secrets.yaml`, its
/// key/value pairs are merged into `options`. Entries already present in
/// `options` win, so an inline override in `config.yaml` beats the shared
/// secret.
pub fn build_operator(cfg: &StoreTierConfig, credentials: &HashMap<String, Credentials>) -> Result<Operator> {
    let scheme = Scheme::from_str(&cfg.backend).map_err(|e| {
        anyhow!(
            "cache store backend '{backend}': {e} \
             (see https://docs.rs/opendal/0.50/opendal/enum.Scheme.html for the full list)",
            backend = cfg.backend
        )
    })?;

    let mut options = cfg.options.clone();
    if let Some(name) = &cfg.credentials {
        let creds = credentials
            .get(name)
            .ok_or_else(|| anyhow!("cache store references credentials '{name}' but secrets.yaml has no such entry"))?;
        for (k, v) in &creds.0 {
            options.entry(k.clone()).or_insert_with(|| v.clone());
        }
    }

    Operator::via_iter(scheme, options)
        .with_context(|| format!("building opendal operator for backend '{}'", cfg.backend))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::OutputFormat;
    use crate::tile::TileCoord;
    use bytes::Bytes;
    use opendal::services;

    fn memory_tier() -> StoreTier {
        StoreTier::new(
            Operator::new(services::Memory::default())
                .expect("memory builder")
                .finish(),
        )
    }

    fn key() -> TileKey {
        TileKey {
            layer: "L".into(),
            grid_dir: "3857".into(),
            coord: TileCoord::new(0, 0, 0),
            format: OutputFormat::Png,
        }
    }

    const TTL: Duration = Duration::from_secs(60);

    #[tokio::test]
    async fn put_then_get_keeps_bytes_and_write_time() {
        let tier = memory_tier();
        let e = Entry {
            bytes: Bytes::from_static(b"abc"),
            written_at: now_unix_seconds() - 10,
        };
        tier.put(&key(), e.clone()).await.expect("put");
        assert_eq!(tier.get(&key(), TTL).await.unwrap(), Some(e));
        assert!(!tier.durable(), "opendal memory is not durable");
    }

    #[tokio::test]
    async fn expired_entry_is_a_miss() {
        let tier = memory_tier();
        let old = Entry {
            bytes: Bytes::from_static(b"abc"),
            written_at: now_unix_seconds() - 120,
        };
        tier.put(&key(), old).await.expect("put");
        assert!(tier.get(&key(), TTL).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn missing_key_returns_none() {
        assert!(memory_tier().get(&key(), TTL).await.unwrap().is_none());
    }

    /// The `fs` backend (the default) is compiled in: tiles land on disk
    /// under `root`, one directory per layer, and the startup check passes.
    #[tokio::test]
    async fn fs_backend_stores_tiles_per_layer_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = StoreTierConfig::default();
        cfg.options
            .insert("root".into(), dir.path().to_string_lossy().into_owned());
        let tier = StoreTier::from_config(&cfg, &HashMap::new()).expect("fs backend builds");
        assert_eq!(tier.name(), "fs");
        assert!(tier.durable());
        tier.check().await.expect("startup check on a writable dir");
        assert!(!dir.path().join(PROBE).exists(), "probe left behind");

        let (mut a, mut b) = (key(), key());
        a.layer = "layer_a".into();
        b.layer = "layer_b".into();
        tier.put(&a, Entry::now(Bytes::from_static(b"A"))).await.unwrap();
        tier.put(&b, Entry::now(Bytes::from_static(b"B"))).await.unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("layer_a/3857/png/0/0/0.png")).unwrap(),
            b"A"
        );
        assert_eq!(
            std::fs::read(dir.path().join("layer_b/3857/png/0/0/0.png")).unwrap(),
            b"B"
        );
        assert_eq!(
            tier.get(&a, TTL).await.unwrap().unwrap().bytes,
            Bytes::from_static(b"A")
        );
    }

    /// `azblob` is compiled in; it only needs its options.
    #[test]
    fn build_operator_knows_azblob() {
        let cfg = StoreTierConfig {
            backend: "azblob".into(),
            options: [
                ("container", "tiles"),
                ("endpoint", "https://acct.blob.core.windows.net"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            credentials: None,
        };
        let tier = StoreTier::from_config(&cfg, &HashMap::new()).expect("azblob backend builds");
        assert_eq!(tier.name(), "azblob");
    }

    /// A backend that can't be written fails the startup check.
    #[tokio::test]
    async fn check_fails_on_unwritable_backend() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let mut cfg = StoreTierConfig::default();
        cfg.options.insert("root".into(), file.to_string_lossy().into_owned());
        let tier = StoreTier::from_config(&cfg, &HashMap::new()).unwrap();
        let err = format!("{:#}", tier.check().await.unwrap_err());
        assert!(err.contains("cannot write"), "{err}");
    }

    #[test]
    fn build_operator_rejects_unknown_scheme() {
        let cfg = StoreTierConfig {
            backend: "definitely-not-a-scheme".into(),
            options: HashMap::new(),
            credentials: None,
        };
        let msg = format!("{:#}", build_operator(&cfg, &HashMap::new()).unwrap_err());
        assert!(
            msg.contains("definitely-not-a-scheme"),
            "error should name the offending backend, got: {msg}"
        );
    }

    #[test]
    fn build_operator_merges_named_credentials_into_options() {
        let mut creds = HashMap::new();
        creds.insert(
            "mycreds".into(),
            Credentials(
                [("from_secrets".to_string(), "value".to_string())]
                    .into_iter()
                    .collect(),
            ),
        );
        let cfg = StoreTierConfig {
            backend: "memory".into(),
            options: HashMap::new(),
            credentials: Some("mycreds".into()),
        };
        build_operator(&cfg, &creds).expect("memory backend with merged creds builds");
    }

    #[test]
    fn build_operator_missing_credentials_block_errors() {
        let cfg = StoreTierConfig {
            backend: "memory".into(),
            options: HashMap::new(),
            credentials: Some("does_not_exist".into()),
        };
        let msg = format!("{:#}", build_operator(&cfg, &HashMap::new()).unwrap_err());
        assert!(
            msg.contains("does_not_exist"),
            "error should name the missing credentials block, got: {msg}"
        );
    }
}
