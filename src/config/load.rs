//! Load + merge `config.yaml`, `secrets.yaml`, env-var overrides, CLI overrides.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde_yaml::{Mapping, Value};

use super::merge::merge;
use super::model::{Config, Secrets};
use super::validate::validate;

/// Read both files, deep-merge them (replacing lists), apply env-var overrides
/// (`KWMS_*`), then run validation. Returns the merged config and the secrets
/// side-channel.
///
/// Behavior:
/// - `config_path` must exist; missing → error.
/// - `secrets_path` is optional; if missing we start from an empty mapping.
/// - Env-var overrides apply to the *config* side only. Any env var starting
///   with `KWMS_` has its suffix split on `__` (double-underscore) into a
///   nested key path, lower-cased. The value is parsed as a YAML scalar (so
///   `"[1024, 1024]"` yields a list, `"true"` yields a bool, etc.).
pub fn load(config_path: &Path, secrets_path: &Path) -> Result<(Config, Secrets)> {
    // -- config side --
    let config_text = std::fs::read_to_string(config_path)
        .with_context(|| format!("reading config file '{}'", config_path.display()))?;
    let mut config_value: Value = serde_yaml::from_str(&config_text)
        .with_context(|| format!("parsing config file '{}'", config_path.display()))?;
    // An empty config.yaml deserializes to Null; treat that as an empty map so
    // we can merge env-var overrides into it.
    if config_value.is_null() {
        config_value = Value::Mapping(Mapping::new());
    }

    // -- secrets side --
    let secrets_value: Value = if secrets_path.exists() {
        let text = std::fs::read_to_string(secrets_path)
            .with_context(|| format!("reading secrets file '{}'", secrets_path.display()))?;
        let mut v: Value = serde_yaml::from_str(&text)
            .with_context(|| format!("parsing secrets file '{}'", secrets_path.display()))?;
        if v.is_null() {
            v = Value::Mapping(Mapping::new());
        }
        v
    } else {
        Value::Mapping(Mapping::new())
    };

    // -- env-var overrides (config side only) --
    let env_overlay = env_overlay_value(std::env::vars_os())?;
    let config_value = merge(config_value, env_overlay).context("applying KWMS_* env-var overrides")?;

    // -- deserialize into typed structs --
    // serde echoes offending values ("invalid type: string \"sv=…\""), and
    // secrets.yaml, env overrides and inline store options hold credentials:
    // errors keep the field path and the problem, not the value.
    let config: Config = serde_yaml::from_value(config_value)
        .map_err(|e| anyhow::anyhow!(without_values(&e.to_string())))
        .with_context(|| format!("validating shape of '{}'", config_path.display()))?;
    let secrets: Secrets = serde_yaml::from_value(secrets_value)
        .map_err(|e| anyhow::anyhow!(without_values(&e.to_string())))
        .with_context(|| format!("validating shape of '{}'", secrets_path.display()))?;

    validate(&config).context("config validation failed")?;
    crate::source::gdal::credentials::plan(&config, &secrets).context("config validation failed")?;
    for layer in &config.layers {
        if let Some(path) = layer.effective_source_mask(config.source_mask.as_deref()) {
            crate::source::gdal::mask::load(path)
                .with_context(|| format!("config validation failed: layer '{}': source_mask", layer.name))?;
        }
    }
    for (i, tier) in config.cache.tiers.iter().enumerate() {
        if let crate::config::TierConfig::Store(st) = tier {
            if let Some(name) = st
                .credentials
                .as_ref()
                .filter(|n| !secrets.credentials.contains_key(*n))
            {
                bail!("config validation failed: cache.tiers[{i}].credentials '{name}' is not defined in secrets.yaml");
            }
        }
    }

    Ok((config, secrets))
}

/// Build a `Value::Mapping` from `KWMS_*` env vars, with `__` denoting nesting.
///
/// Only variables whose first key is a top-level config section count:
/// Kubernetes injects `KWMS_PORT`, `KWMS_SERVICE_HOST`, … into every pod of
/// a namespace with a Service named `kwms`; those are skipped (logged by
/// name only). Names and values must be UTF-8.
fn env_overlay_value(vars: impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)>) -> Result<Value> {
    let sections = config_sections();
    let mut overlay = Value::Mapping(Mapping::new());
    for (k, v) in vars {
        let Some(k) = k.to_str() else { continue };
        let Some(suffix) = k.strip_prefix("KWMS_") else {
            continue;
        };
        if suffix.is_empty() {
            continue;
        }
        let path: Vec<String> = suffix.split("__").map(|s| s.to_lowercase()).collect();
        if path.iter().any(|p| p.is_empty()) {
            // Skip pathologically formed names like `KWMS___FOO`.
            continue;
        }
        if !path.first().is_some_and(|s| sections.contains(s)) {
            tracing::info!(var = %k, "ignoring env var: not a config override (KWMS_<SECTION>__<KEY>)");
            continue;
        }
        // Don't echo the value: env vars routinely carry secrets.
        let v = v
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("env-var '{k}' is not valid UTF-8"))?;
        let parsed_value: Value =
            serde_yaml::from_str(v).with_context(|| format!("parsing env-var '{k}' value as YAML scalar"))?;
        let nested = nest(&path, parsed_value);
        overlay = merge(overlay, nested)?;
    }
    Ok(overlay)
}

/// Top-level keys of `config.yaml` (from the config schema).
fn config_sections() -> Vec<String> {
    let schema = schemars::schema_for!(Config);
    schema
        .schema
        .object
        .map(|o| o.properties.keys().cloned().collect())
        .unwrap_or_default()
}

/// A serde error message without the values it quotes: `invalid type:
/// string "abc", expected …` → `invalid type: string <value>, expected …`.
/// Field and variant names (in backticks after "field"/"variant") stay.
fn without_values(message: &str) -> String {
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    while let Some(i) = rest.find(['"', '`']) {
        let quote = rest[i..].chars().next().unwrap_or('"');
        let before = &rest[..i];
        let after = &rest[i.saturating_add(1)..];
        let Some(end) = after.find(quote) else {
            out.push_str(rest);
            return out;
        };
        out.push_str(before);
        let keeps_name = ["field ", "variant ", "fields ", "variants ", "of "]
            .iter()
            .any(|w| before.ends_with(w))
            || before.ends_with(", ");
        if quote == '`' && keeps_name {
            out.push('`');
            out.push_str(&after[..end]);
            out.push('`');
        } else {
            out.push_str("<value>");
        }
        rest = &after[end.saturating_add(1)..];
    }
    out.push_str(rest);
    out
}

/// Wrap `value` in nested mappings keyed by `path`.
/// e.g. `nest(&["server", "bind"], "...")` → `{server: {bind: "..."}}`.
fn nest(path: &[String], value: Value) -> Value {
    let mut cur = value;
    for key in path.iter().rev() {
        let mut m = Mapping::new();
        m.insert(Value::String(key.clone()), cur);
        cur = Value::Mapping(m);
    }
    cur
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::sync::Mutex;
    use tempfile::NamedTempFile;

    // Tests in this module mutate process-global env vars. Run them under a
    // mutex to avoid races between parallel test threads.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Guard that removes an env var on drop, so each test cleans up after
    /// itself regardless of pass/fail.
    struct EnvGuard {
        keys: Vec<String>,
    }
    impl EnvGuard {
        fn new() -> Self {
            Self { keys: Vec::new() }
        }
        fn set(&mut self, k: &str, v: &str) {
            std::env::set_var(k, v);
            self.keys.push(k.to_string());
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            for k in &self.keys {
                std::env::remove_var(k);
            }
        }
    }

    fn write_tmp(contents: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all(contents.as_bytes()).expect("write");
        f.flush().expect("flush");
        f
    }

    const MINIMAL_CONFIG: &str = r#"
server:
  bind: "0.0.0.0:8080"
layers:
  - name: ortho2024
    grids: [webmercator]
    sources:
      - uri: "vsiaz://photos/x.vrt"
    title: "Ortho 2024"
    formats: [png]
"#;

    #[test]
    fn loads_minimal_config_without_secrets_file() {
        let _g = ENV_LOCK.lock().unwrap();
        let cfg_file = write_tmp(MINIMAL_CONFIG);
        let missing_secrets = std::env::temp_dir().join("definitely-not-here-kwms.yaml");
        // Ensure it really doesn't exist.
        let _ = std::fs::remove_file(&missing_secrets);

        let (config, secrets) = load(cfg_file.path(), &missing_secrets).expect("load should succeed");
        assert_eq!(config.server.bind, "0.0.0.0:8080");
        assert_eq!(config.layers.len(), 1);
        assert_eq!(config.layers[0].name, "ortho2024");
        assert!(secrets.credentials.is_empty());
    }

    #[test]
    fn env_var_overrides_scalar() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("KWMS_SERVER__BIND", "127.0.0.1:9090");

        let cfg_file = write_tmp(MINIMAL_CONFIG);
        let no_secrets = std::env::temp_dir().join("nope-kwms-secrets.yaml");
        let _ = std::fs::remove_file(&no_secrets);

        let (config, _) = load(cfg_file.path(), &no_secrets).expect("load");
        assert_eq!(config.server.bind, "127.0.0.1:9090");
    }

    #[test]
    fn env_var_overrides_list_via_yaml_scalar() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut env = EnvGuard::new();
        env.set("KWMS_SERVER__MAX_REQUEST_PIXELS", "[1024, 1024]");

        let cfg_file = write_tmp(MINIMAL_CONFIG);
        let no_secrets = std::env::temp_dir().join("nope-kwms-secrets2.yaml");
        let _ = std::fs::remove_file(&no_secrets);

        let (config, _) = load(cfg_file.path(), &no_secrets).expect("load");
        assert_eq!(config.server.max_request_pixels, [1024, 1024]);
    }

    #[test]
    fn secrets_file_loaded_when_present() {
        let _g = ENV_LOCK.lock().unwrap();
        let cfg_file = write_tmp(MINIMAL_CONFIG);
        let secrets_text = r#"
credentials:
  pdok_blob:
    AZURE_STORAGE_ACCOUNT: "luchtfoto"
    AZURE_STORAGE_ACCESS_KEY: "abc"
"#;
        let sec_file = write_tmp(secrets_text);
        let (_config, secrets) = load(cfg_file.path(), sec_file.path()).expect("load with secrets");
        let creds = secrets.credentials.get("pdok_blob").expect("pdok_blob present");
        assert_eq!(
            creds.0.get("AZURE_STORAGE_ACCOUNT").map(String::as_str),
            Some("luchtfoto")
        );
    }

    #[test]
    fn missing_config_file_errors() {
        let _g = ENV_LOCK.lock().unwrap();
        let nowhere = std::env::temp_dir().join("kwms-does-not-exist-config.yaml");
        let _ = std::fs::remove_file(&nowhere);
        let no_secrets = std::env::temp_dir().join("kwms-also-no-secrets.yaml");
        let _ = std::fs::remove_file(&no_secrets);
        let err = load(&nowhere, &no_secrets).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("reading config file"), "got: {s}");
    }

    #[test]
    fn validation_failure_propagates() {
        let _g = ENV_LOCK.lock().unwrap();
        // Layer with empty formats — should fail validation.
        let bad = r#"
layers:
  - name: bad
    grids: [webmercator]
    sources:
      - uri: "x"
    title: "T"
    formats: []
"#;
        let cfg_file = write_tmp(bad);
        let no_secrets = std::env::temp_dir().join("nope-kwms-validation.yaml");
        let _ = std::fs::remove_file(&no_secrets);
        let err = load(cfg_file.path(), &no_secrets).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("formats must be non-empty"), "got: {s}");
    }

    #[test]
    fn env_overlay_is_empty_with_no_kwms_vars() {
        let _g = ENV_LOCK.lock().unwrap();
        // We can't guarantee absence of pre-existing KWMS_* vars in the
        // ambient env, but the helper itself should at minimum produce a
        // mapping (never error in a clean state).
        let v = env_overlay_value(std::iter::empty()).expect("overlay");
        assert_eq!(v, Value::Mapping(Mapping::new()));
    }

    fn vars(pairs: &[(&str, &str)]) -> impl Iterator<Item = (std::ffi::OsString, std::ffi::OsString)> {
        pairs
            .iter()
            .map(|(k, v)| (k.into(), v.into()))
            .collect::<Vec<_>>()
            .into_iter()
    }

    /// What Kubernetes injects for a Service named `kwms` is ignored; real
    /// overrides still apply.
    #[test]
    fn kubernetes_service_links_are_ignored() {
        let v = env_overlay_value(vars(&[
            ("KWMS_PORT", "tcp://10.0.0.1:8080"),
            ("KWMS_PORT_8080_TCP_ADDR", "10.0.0.1"),
            ("KWMS_SERVICE_HOST", "10.0.0.1"),
            ("KWMS_SERVICE_PORT_HTTP", "8080"),
            ("KWMS_SERVER__BIND", "127.0.0.1:9000"),
            ("OTHER", "x"),
        ]))
        .unwrap();
        let expected: Value = serde_yaml::from_str("server:\n  bind: 127.0.0.1:9000\n").unwrap();
        assert_eq!(v, expected);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_override_is_an_error_without_its_value() {
        use std::os::unix::ffi::OsStringExt;
        let bad = std::ffi::OsString::from_vec(vec![b's', 0xff, b'x']);
        let err = env_overlay_value(std::iter::once(("KWMS_SERVER__BIND".into(), bad))).unwrap_err();
        assert_eq!(err.to_string(), "env-var 'KWMS_SERVER__BIND' is not valid UTF-8");
        // Not an override: skipped, whatever it holds.
        let other = std::ffi::OsString::from_vec(vec![0xff]);
        assert!(env_overlay_value(std::iter::once(("PATH".into(), other))).is_ok());
    }

    #[test]
    fn shape_errors_keep_names_but_not_values() {
        assert_eq!(
            without_values(r#"credentials.a: invalid type: string "sv=1&sig=SECRET", expected a map"#),
            "credentials.a: invalid type: string <value>, expected a map"
        );
        assert_eq!(
            without_values("invalid type: integer `123456`, expected a string"),
            "invalid type: integer <value>, expected a string"
        );
        assert_eq!(
            without_values("server: unknown field `prot`, expected one of `bind`, `public_url`"),
            "server: unknown field `prot`, expected one of `bind`, `public_url`"
        );
        assert_eq!(without_values("unknown variant `jpg`"), "unknown variant `jpg`");
    }

    #[test]
    fn secrets_file_errors_do_not_echo_values() {
        let _g = ENV_LOCK.lock().unwrap();
        let mut cfg = NamedTempFile::new().unwrap();
        writeln!(cfg, "layers: []").unwrap();
        let mut sec = NamedTempFile::new().unwrap();
        writeln!(sec, "credentials:\n  a: \"sv=2024&sig=TOPSECRET\"").unwrap();
        let err = format!("{:#}", load(cfg.path(), sec.path()).unwrap_err());
        assert!(!err.contains("TOPSECRET"), "{err}");
        assert!(err.contains("invalid type"), "{err}");
    }

    #[test]
    fn nest_builds_nested_mapping() {
        let v = nest(&["server".into(), "bind".into()], Value::String("x".into()));
        let expected: Value = serde_yaml::from_str("server:\n  bind: x\n").unwrap();
        assert_eq!(v, expected);
    }
}
