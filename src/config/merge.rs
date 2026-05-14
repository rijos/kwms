//! Deep-merge YAML values — maps merged recursively, lists replaced wholesale.

use anyhow::Result;
use serde_yaml::Value;

/// Merge `b` into `a`, returning the combined value. Higher-precedence values
/// (`b`) win on conflicts.
pub fn merge(_a: Value, _b: Value) -> Result<Value> {
    todo!("phase 2 (config agent)")
}
