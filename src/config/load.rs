//! Load + merge `config.yaml`, `secrets.yaml`, env-var overrides, CLI overrides.

use std::path::Path;

use anyhow::Result;

use super::model::{Config, Secrets};

/// Read both files, deep-merge them (replacing lists), apply env-var overrides
/// (`KWMS_*`), then run validation. Returns the merged config and the secrets
/// side-channel.
pub fn load(_config: &Path, _secrets: &Path) -> Result<(Config, Secrets)> {
    todo!("phase 2 (config agent)")
}
