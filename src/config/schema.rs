//! JSON Schema export for `Config`.

use anyhow::Result;
use schemars::schema_for;

use super::model::Config;

pub fn emit_json_schema() -> Result<String> {
    let schema = schema_for!(Config);
    Ok(serde_json::to_string_pretty(&schema)?)
}
