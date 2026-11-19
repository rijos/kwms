//! `kwms config check` — load + merge + validate, exit non-zero on failure.
//!
//! All work is delegated to [`crate::config::load::load`], which already
//! reads both files, applies env-var overlay, deserialises into typed
//! structs, and runs validation. We just turn its `Result` into a tidy
//! stdout summary on success and let `anyhow::Error` propagate to the
//! process exit on failure.

use std::path::PathBuf;

use anyhow::Context;

pub fn run(config: PathBuf, secrets: PathBuf) -> anyhow::Result<()> {
    let (cfg, sec) = crate::config::load::load(&config, &secrets).with_context(|| {
        format!(
            "checking config '{}' (secrets '{}')",
            config.display(),
            secrets.display(),
        )
    })?;

    println!("ok");
    println!("  bind:               {}", cfg.server.bind);
    println!("  log_level:          {}", cfg.server.log_level);
    println!("  cache tiers:        {}", cfg.cache.tiers.len());
    for tier in &cfg.cache.tiers {
        match tier {
            crate::config::TierConfig::Memory(m) => println!("    - memory (max_bytes={})", m.max_bytes),
            crate::config::TierConfig::Store(st) => println!(
                "    - store backend={} ({} option(s){})",
                st.backend,
                st.options.len(),
                st.credentials
                    .as_ref()
                    .map(|c| format!(", credentials='{c}'"))
                    .unwrap_or_default(),
            ),
        }
    }
    println!("  custom grids:       {}", cfg.grids.len());
    println!("  credential blocks:  {}", sec.credentials.len());
    println!("  layers:             {}", cfg.layers.len());
    for layer in &cfg.layers {
        println!(
            "    - {} (grids={:?}, formats={:?}, resampling={})",
            layer.name, layer.grids, layer.formats, layer.resampling,
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    fn write_tmp(contents: &str) -> NamedTempFile {
        let mut f = NamedTempFile::new().expect("tempfile");
        f.write_all(contents.as_bytes()).expect("write");
        f.flush().expect("flush");
        f
    }

    const GOOD_CONFIG: &str = r#"
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
    fn returns_ok_for_valid_config() {
        let cfg = write_tmp(GOOD_CONFIG);
        let no_secrets = std::env::temp_dir().join("kwms-check-no-such-secrets.yaml");
        let _ = std::fs::remove_file(&no_secrets);
        run(cfg.path().to_path_buf(), no_secrets).expect("valid config must check ok");
    }

    #[test]
    fn returns_err_for_invalid_config() {
        // Empty `formats:` is rejected by the validator.
        let bad = r#"
layers:
  - name: bad
    grids: [webmercator]
    sources:
      - uri: "x"
    title: "T"
    formats: []
"#;
        let cfg = write_tmp(bad);
        let no_secrets = std::env::temp_dir().join("kwms-check-bad-no-secrets.yaml");
        let _ = std::fs::remove_file(&no_secrets);
        let err = run(cfg.path().to_path_buf(), no_secrets).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("formats must be non-empty"), "got: {s}");
    }

    #[test]
    fn returns_err_for_missing_config_file() {
        let nowhere = std::env::temp_dir().join("kwms-check-missing-config.yaml");
        let _ = std::fs::remove_file(&nowhere);
        let no_secrets = std::env::temp_dir().join("kwms-check-missing-secrets.yaml");
        let _ = std::fs::remove_file(&no_secrets);
        let err = run(nowhere, no_secrets).unwrap_err();
        let s = format!("{err:#}");
        assert!(s.contains("reading config file"), "got: {s}");
    }
}
