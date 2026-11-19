//! Source credentials as GDAL **path-specific options**.
//!
//! A credentials block from `secrets.yaml` (e.g. `AZURE_STORAGE_ACCOUNT`,
//! `AZURE_STORAGE_SAS_TOKEN`) is registered with GDAL for one object-store
//! path prefix (`VSISetPathSpecificOption`), e.g. `/vsiaz/open-tiles/`. GDAL
//! then uses it for every file under that prefix and nowhere else — also
//! for files a dataset opens itself (the tiles of a `GTI:` index or a VRT)
//! and on GDAL's own worker threads. Neither alternative does both:
//! dataset open options are not where GDAL looks for VSI credentials, and
//! thread-local config options do not reach GDAL's worker threads.
//!
//! Registration happens once, at startup, before any source is opened.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context, Result};

use crate::config::{Config, Credentials, Secrets, SourceConfig};

/// Object-store handlers whose paths look like `/vsiXXX/<bucket>/<key>`, so a
/// default prefix can be derived from a source URI.
const BUCKET_HANDLERS: &[&str] = &["vsiaz", "vsiadls", "vsis3", "vsigs", "vsioss", "vsiswift"];

/// Every path prefix that gets credentials, mapped to the block's name and
/// contents. Fails if a source references a missing block, has no usable
/// prefix, or two blocks claim the same prefix.
pub fn plan<'a>(config: &Config, secrets: &'a Secrets) -> Result<BTreeMap<String, (String, &'a Credentials)>> {
    let mut scopes: BTreeMap<String, (String, &'a Credentials)> = BTreeMap::new();
    for layer in &config.layers {
        for source in &layer.sources {
            let Some(name) = &source.credentials else {
                if source.credentials_prefix.is_some() {
                    bail!("layer '{}': source '{}' sets credentials_prefix without credentials", layer.name, source.uri);
                }
                continue;
            };
            let ctx = || format!("layer '{}': source '{}'", layer.name, source.uri);
            let block = secrets
                .credentials
                .get(name)
                .ok_or_else(|| anyhow!("credentials '{name}' are not defined in secrets.yaml"))
                .with_context(ctx)?;
            check_block(block).with_context(|| format!("credentials '{name}'")).with_context(ctx)?;
            let prefix = prefix_for(source).with_context(ctx)?;
            match scopes.get(&prefix) {
                Some((other, _)) if other != name => bail!(
                    "{}: path prefix '{prefix}' already has credentials '{other}'; \
                     one prefix can only carry one credentials block",
                    ctx()
                ),
                _ => {
                    scopes.insert(prefix, (name.clone(), block));
                }
            }
        }
    }
    Ok(scopes)
}

/// Register every source's credentials with GDAL ([`plan`] first).
///
/// Each (prefix, option) is written at most once per process: GDAL hands
/// out pointers into the stored values without holding its lock, so
/// changing a value that a reader may hold is never allowed. Registering the
/// same value again (e.g. a second `AppState` in tests) is a no-op;
/// registering a different one is an error.
pub fn register(config: &Config, secrets: &Secrets) -> Result<()> {
    static REGISTERED: Mutex<BTreeMap<(String, String), String>> = Mutex::new(BTreeMap::new());

    let scopes = plan(config, secrets)?;
    let mut registered = REGISTERED.lock().map_err(|_| anyhow!("credentials registry poisoned"))?;
    for (prefix, (name, block)) in scopes {
        // Sorted, so the order of GDAL calls doesn't depend on hashing.
        let options: BTreeMap<&String, &String> = block.0.iter().collect();
        for (key, value) in options {
            let slot = (prefix.clone(), key.to_ascii_uppercase());
            match registered.get(&slot) {
                Some(existing) if existing == value => continue,
                Some(_) => bail!(
                    "credentials '{name}': option {key} for '{prefix}' was already registered with a \
                     different value; GDAL path-specific options cannot change while serving"
                ),
                None => {}
            }
            crate::ffi::vsi_set_path_specific_option(&prefix, key, value)
                .with_context(|| format!("credentials '{name}': registering option {key} for '{prefix}'"))?;
            registered.insert(slot, value.clone());
        }
        tracing::info!(credentials = %name, prefix = %prefix, "registered source credentials");
    }
    Ok(())
}

/// The prefix `source`'s credentials apply to: `credentials_prefix` if set,
/// else `/vsiXXX/<bucket>/` from the URI. Always ends in `/`, so
/// `/vsis3/foo/` never matches bucket `foobar`.
fn prefix_for(source: &SourceConfig) -> Result<String> {
    if let Some(prefix) = &source.credentials_prefix {
        let (handler, bucket) = split_vsi(prefix)
            .ok_or_else(|| anyhow!("credentials_prefix '{prefix}' must look like /vsiXXX/<bucket>/…"))?;
        if !handler.chars().all(|c| c.is_ascii_lowercase() || c == '_') || bucket.is_empty() {
            bail!("credentials_prefix '{prefix}' must look like /vsiXXX/<bucket>/…");
        }
        return Ok(if prefix.ends_with('/') { prefix.clone() } else { format!("{prefix}/") });
    }
    match split_vsi(&source.uri) {
        Some((handler, bucket)) if BUCKET_HANDLERS.contains(&handler) && !bucket.is_empty() => {
            Ok(format!("/{handler}/{bucket}/"))
        }
        _ => bail!(
            "cannot tell which object-store paths the credentials are for; set credentials_prefix \
             (e.g. /vsiaz/<container>/) — the URI is not a /vsiaz/, /vsis3/, … path"
        ),
    }
}

/// `/vsiXXX/<bucket>/…` → (`vsiXXX`, `<bucket>`).
fn split_vsi(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix('/')?;
    let (handler, rest) = rest.split_once('/')?;
    if !handler.starts_with("vsi") || handler.len() == 3 {
        return None;
    }
    let bucket = rest.split('/').next().unwrap_or_default();
    Some((handler, bucket))
}

/// Option names must be plain GDAL option names; nothing may contain NUL.
fn check_block(block: &Credentials) -> Result<()> {
    let block: &HashMap<String, String> = &block.0;
    if block.is_empty() {
        bail!("block is empty");
    }
    for (key, value) in block {
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!("'{key}' is not a GDAL option name");
        }
        if value.contains('\0') {
            bail!("value of {key} contains a NUL byte");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(uri: &str, credentials: Option<&str>, prefix: Option<&str>) -> SourceConfig {
        SourceConfig {
            uri: uri.into(),
            credentials: credentials.map(Into::into),
            credentials_prefix: prefix.map(Into::into),
            ..Default::default()
        }
    }

    #[test]
    fn prefix_is_derived_from_bucket_uris_only() {
        assert_eq!(prefix_for(&source("/vsiaz/photos/a/b.tif", Some("c"), None)).unwrap(), "/vsiaz/photos/");
        assert_eq!(prefix_for(&source("/vsis3/bkt/x.vrt", Some("c"), None)).unwrap(), "/vsis3/bkt/");
        for uri in ["data/tiles.shp", "GTI:data/tiles.shp", "/vsicurl/https://h/x.tif", "/vsiaz/", "/vsi/x/y"] {
            assert!(prefix_for(&source(uri, Some("c"), None)).is_err(), "{uri}");
        }
    }

    #[test]
    fn explicit_prefix_is_checked_and_gets_a_trailing_slash() {
        let s = source("GTI:data/tiles.shp", Some("c"), Some("/vsiaz/open-tiles"));
        assert_eq!(prefix_for(&s).unwrap(), "/vsiaz/open-tiles/");
        let s = source("GTI:data/tiles.shp", Some("c"), Some("/vsiaz/open-tiles/2026/"));
        assert_eq!(prefix_for(&s).unwrap(), "/vsiaz/open-tiles/2026/");
        for bad in ["vsiaz/x/", "/vsiaz//", "/tmp/x/", "/VSIAZ/x/"] {
            assert!(prefix_for(&source("x", Some("c"), Some(bad))).is_err(), "{bad}");
        }
    }

    fn config_with(sources: Vec<SourceConfig>) -> Config {
        let yaml = r#"
layers:
  - name: l
    grids: [webmercator]
    sources: [{uri: x}]
    title: t
    formats: [png]
"#;
        let mut config: Config = serde_yaml::from_str(yaml).unwrap();
        config.layers[0].sources = sources;
        config
    }

    fn secrets(blocks: &[(&str, &[(&str, &str)])]) -> Secrets {
        let mut s = Secrets::default();
        for (name, kv) in blocks {
            let map = kv.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
            s.credentials.insert(name.to_string(), Credentials(map));
        }
        s
    }

    #[test]
    fn plan_rejects_missing_blocks_and_conflicting_prefixes() {
        let sec = secrets(&[("a", &[("AZURE_STORAGE_ACCOUNT", "x")]), ("b", &[("AZURE_STORAGE_ACCOUNT", "y")])]);

        let missing = config_with(vec![source("/vsiaz/c/x.tif", Some("nope"), None)]);
        let err = format!("{:#}", plan(&missing, &sec).unwrap_err());
        assert!(err.contains("'nope' are not defined"), "{err}");

        let clash = config_with(vec![
            source("/vsiaz/c/x.tif", Some("a"), None),
            source("GTI:t.shp", Some("b"), Some("/vsiaz/c/")),
        ]);
        let err = format!("{:#}", plan(&clash, &sec).unwrap_err());
        assert!(err.contains("already has credentials 'a'"), "{err}");

        let shared = config_with(vec![
            source("/vsiaz/c/x.tif", Some("a"), None),
            source("/vsiaz/c/y.tif", Some("a"), None),
            source("/vsiaz/d/y.tif", Some("b"), None),
        ]);
        let scopes = plan(&shared, &sec).unwrap();
        assert_eq!(scopes.keys().collect::<Vec<_>>(), ["/vsiaz/c/", "/vsiaz/d/"]);

        let orphan_prefix = config_with(vec![source("x.tif", None, Some("/vsiaz/c/"))]);
        assert!(plan(&orphan_prefix, &sec).is_err());
    }

    #[test]
    fn plan_rejects_malformed_blocks() {
        for bad in [&[][..], &[("NOT=OK", "v")][..], &[("K", "a\0b")][..]] {
            let sec = secrets(&[("a", bad)]);
            let cfg = config_with(vec![source("/vsiaz/c/x.tif", Some("a"), None)]);
            assert!(plan(&cfg, &sec).is_err(), "{bad:?}");
        }
    }

    /// Registered credentials reach GDAL for paths under the prefix only,
    /// and can't be changed afterwards.
    #[test]
    fn register_sets_gdal_path_specific_options_once() {
        let prefix = "/vsiaz/kwms-credentials-test/";
        let cfg = config_with(vec![source("GTI:t.shp", Some("a"), Some(prefix))]);
        let sec = secrets(&[("a", &[("AZURE_STORAGE_ACCOUNT", "kwmstestacct")])]);
        register(&cfg, &sec).unwrap();
        register(&cfg, &sec).unwrap(); // same value again: fine

        // What GDAL's VSI handlers see (falls back to the process-wide
        // config option, unset here).
        let get = |path| crate::ffi::vsi_get_path_specific_option(path, "azure_storage_account").unwrap();
        assert_eq!(get("/vsiaz/kwms-credentials-test/a/b.tif").as_deref(), Some("kwmstestacct"));
        assert_eq!(get("/vsiaz/kwms-credentials-test-other/b.tif"), None);

        let changed = secrets(&[("a", &[("AZURE_STORAGE_ACCOUNT", "other")])]);
        let err = format!("{:#}", register(&cfg, &changed).unwrap_err());
        assert!(err.contains("already registered with a different value"), "{err}");
    }
}
