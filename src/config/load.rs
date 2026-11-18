//! Load + merge `config.yaml`, `secrets.yaml`, env-var overrides, CLI overrides.

use std::path::Path;

use anyhow::{Context, Result};
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
    let env_overlay = env_overlay_value()?;
    let config_value =
        merge(config_value, env_overlay).context("applying KWMS_* env-var overrides")?;

    // -- deserialize into typed structs --
    let config: Config = serde_yaml::from_value(config_value)
        .with_context(|| format!("validating shape of '{}'", config_path.display()))?;
    let secrets: Secrets = serde_yaml::from_value(secrets_value).with_context(|| {
        format!(
            "validating shape of '{}'",
            secrets_path.display()
        )
    })?;

    validate(&config).context("config validation failed")?;

    Ok((config, secrets))
}

/// Build a `Value::Mapping` from `KWMS_*` env vars, with `__` denoting nesting.
fn env_overlay_value() -> Result<Value> {
    let mut overlay = Value::Mapping(Mapping::new());
    for (k, v) in std::env::vars() {
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
        let parsed_value: Value = serde_yaml::from_str(&v).with_context(|| {
            // Don't echo the value: env vars routinely carry secrets.
            format!("parsing env-var '{k}' value as YAML scalar")
        })?;
        let nested = nest(&path, parsed_value);
        overlay = merge(overlay, nested)?;
    }
    Ok(overlay)
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

        let (config, secrets) =
            load(cfg_file.path(), &missing_secrets).expect("load should succeed");
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
        let (_config, secrets) =
            load(cfg_file.path(), sec_file.path()).expect("load with secrets");
        let creds = secrets
            .credentials
            .get("pdok_blob")
            .expect("pdok_blob present");
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
        let v = env_overlay_value().expect("overlay");
        assert!(matches!(v, Value::Mapping(_)));
    }

    #[test]
    fn nest_builds_nested_mapping() {
        let v = nest(
            &["server".into(), "bind".into()],
            Value::String("x".into()),
        );
        let expected: Value =
            serde_yaml::from_str("server:\n  bind: x\n").unwrap();
        assert_eq!(v, expected);
    }
}
