//! Deep-merge YAML values — maps merged recursively, lists replaced wholesale.

use anyhow::Result;
use serde_yaml::{Mapping, Value};

/// Merge `b` into `a`, returning the combined value. Higher-precedence values
/// (`b`) win on conflicts.
///
/// Semantics:
/// - If both `a` and `b` are mappings: keys are unioned and overlapping keys
///   are merged recursively.
/// - If either side is a sequence: `b` replaces `a` wholesale.
/// - Otherwise (scalars, nulls, or type mismatches): `b` wins.
pub fn merge(a: Value, b: Value) -> Result<Value> {
    match (a, b) {
        // Both mappings → deep merge.
        (Value::Mapping(a_map), Value::Mapping(b_map)) => {
            let mut out = a_map;
            for (k, b_v) in b_map {
                match out.remove(&k) {
                    Some(a_v) => {
                        let merged = merge(a_v, b_v)?;
                        out.insert(k, merged);
                    }
                    None => {
                        out.insert(k, b_v);
                    }
                }
            }
            Ok(Value::Mapping(out))
        }
        // If either side is a sequence (or mismatch), b wins outright.
        // Also: scalars, nulls, and type mismatches — b wins.
        (_, b) => Ok(b),
    }
}

/// Helper: build a `Value::Mapping` from owned key/value pairs.
#[allow(dead_code)]
pub(crate) fn map_from_pairs(pairs: Vec<(Value, Value)>) -> Value {
    let mut m = Mapping::with_capacity(pairs.len());
    for (k, v) in pairs {
        m.insert(k, v);
    }
    Value::Mapping(m)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_yaml::from_str;

    fn y(s: &str) -> Value {
        from_str(s).expect("test yaml parses")
    }

    #[test]
    fn deep_map_merge_unions_keys() {
        let a = y("a: 1\nb:\n  x: 1\n  y: 2\n");
        let b = y("b:\n  y: 99\n  z: 3\nc: 4\n");
        let merged = merge(a, b).unwrap();
        let expected = y("a: 1\nb:\n  x: 1\n  y: 99\n  z: 3\nc: 4\n");
        assert_eq!(merged, expected);
    }

    #[test]
    fn list_is_replaced_wholesale() {
        let a = y("layers: [one, two, three]\n");
        let b = y("layers: [only]\n");
        let merged = merge(a, b).unwrap();
        let expected = y("layers: [only]\n");
        assert_eq!(merged, expected);
    }

    #[test]
    fn scalar_override_b_wins() {
        let a = y("bind: 0.0.0.0:8080\n");
        let b = y("bind: 127.0.0.1:9090\n");
        let merged = merge(a, b).unwrap();
        let expected = y("bind: 127.0.0.1:9090\n");
        assert_eq!(merged, expected);
    }

    #[test]
    fn type_mismatch_b_wins() {
        let a = y("server:\n  bind: 0.0.0.0:8080\n");
        // server is now a scalar in b — b wins outright.
        let b = y("server: disabled\n");
        let merged = merge(a, b).unwrap();
        assert_eq!(merged, y("server: disabled\n"));
    }

    #[test]
    fn missing_keys_kept_from_a() {
        let a = y("a: 1\nb: 2\n");
        let b = y("c: 3\n");
        let merged = merge(a, b).unwrap();
        assert_eq!(merged, y("a: 1\nb: 2\nc: 3\n"));
    }

    #[test]
    fn list_in_a_replaced_by_scalar_in_b() {
        // If `a` is a list, `b` replaces it (per the docs: list replacement).
        let a = y("xs: [1, 2, 3]\n");
        let b = y("xs: 5\n");
        let merged = merge(a, b).unwrap();
        assert_eq!(merged, y("xs: 5\n"));
    }

    #[test]
    fn nested_deep_merge() {
        let a = y("server:\n  bind: 0.0.0.0:8080\n  log_level: info\ncache:\n  hot:\n    max_bytes: 512MB\n");
        let b = y("server:\n  bind: 127.0.0.1:9000\ncache:\n  hot:\n    max_bytes: 1GB\n");
        let merged = merge(a, b).unwrap();
        let expected = y(
            "server:\n  bind: 127.0.0.1:9000\n  log_level: info\ncache:\n  hot:\n    max_bytes: 1GB\n",
        );
        assert_eq!(merged, expected);
    }
}
