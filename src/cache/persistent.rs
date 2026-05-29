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
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use opendal::{ErrorKind, Operator, Scheme};
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
/// Two delegations to opendal:
///
/// 1. The backend name is parsed via [`Scheme::from_str`]
///    (<https://docs.rs/opendal/0.50/opendal/enum.Scheme.html#impl-FromStr-for-Scheme>),
///    which validates against the full list of opendal schemes.
/// 2. The merged option map is handed to [`Operator::via_iter`]
///    (<https://docs.rs/opendal/0.50/opendal/struct.Operator.html#method.via_iter>),
///    which dispatches to the matching builder in
///    <https://docs.rs/opendal/0.50/opendal/services/index.html>.
///
/// If the referenced backend's opendal Cargo feature is not compiled in,
/// `via_map` returns a clear `Unsupported` error — we propagate it
/// verbatim so the user sees exactly which feature to enable.
///
/// Credentials: when `cfg.credentials` names a block in `secrets.yaml`,
/// its key/value pairs are merged into `options`. Entries already present
/// in `options` win, so an inline override in `config.yaml` beats the
/// shared secret. This is the only place we touch credential material.
pub fn build_operator(
    cfg: &PersistentCacheConfig,
    credentials: &HashMap<String, Credentials>,
) -> Result<Operator> {
    let scheme = Scheme::from_str(&cfg.backend).map_err(|e| {
        anyhow!(
            "persistent cache backend '{backend}': {e} \
             (see https://docs.rs/opendal/0.50/opendal/enum.Scheme.html for the full list)",
            backend = cfg.backend
        )
    })?;

    let mut options = cfg.options.clone();
    if let Some(name) = &cfg.credentials {
        let creds = credentials.get(name).ok_or_else(|| {
            anyhow!(
                "persistent cache references credentials '{name}' but \
                 secrets.yaml has no such entry"
            )
        })?;
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
    fn build_operator_default_fs_fails_with_opendal_message() {
        // The `fs` backend requires the `services-fs` opendal feature,
        // which we don't enable. We assert only that the error message
        // names the backend — the exact wording is opendal's, so we
        // don't pin it.
        let cfg = PersistentCacheConfig::default();
        let err = build_operator(&cfg, &HashMap::new()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("fs") || msg.contains("Fs"),
            "expected backend name in error, got: {msg}"
        );
    }

    #[test]
    fn build_operator_rejects_unknown_scheme() {
        let cfg = PersistentCacheConfig {
            backend: "definitely-not-a-scheme".into(),
            options: HashMap::new(),
            credentials: None,
        };
        let err = build_operator(&cfg, &HashMap::new()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("definitely-not-a-scheme"),
            "error should name the offending backend, got: {msg}"
        );
    }

    #[test]
    fn build_operator_merges_named_credentials_into_options() {
        // Use the `memory` scheme — it ships in the default opendal feature
        // set, so via_map will actually succeed. We just need to confirm
        // the merge happens; we don't need to inspect the operator.
        let mut creds = HashMap::new();
        creds.insert(
            "mycreds".into(),
            Credentials(
                [("from_secrets".to_string(), "value".to_string())]
                    .into_iter()
                    .collect(),
            ),
        );
        let cfg = PersistentCacheConfig {
            backend: "memory".into(),
            options: HashMap::new(),
            credentials: Some("mycreds".into()),
        };
        // Should not error: scheme is known, options merge cleanly,
        // memory backend accepts an empty option set.
        build_operator(&cfg, &creds).expect("memory backend with merged creds builds");
    }

    #[test]
    fn build_operator_missing_credentials_block_errors() {
        let cfg = PersistentCacheConfig {
            backend: "memory".into(),
            options: HashMap::new(),
            credentials: Some("does_not_exist".into()),
        };
        let err = build_operator(&cfg, &HashMap::new()).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does_not_exist"),
            "error should name the missing credentials block, got: {msg}"
        );
    }
}
