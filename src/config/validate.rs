//! Validation pass — structural + CRS-aware semantic checks.

use anyhow::Result;

use super::model::Config;

/// Run all validation rules. Errors include file path + YAML location when
/// available.
pub fn validate(_config: &Config) -> Result<()> {
    todo!("phase 2 (config agent)")
}
