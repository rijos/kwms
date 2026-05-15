//! Resolve the effective [`CacheMode`] for a given tile level.
//!
//! See `docs/src/caching.md` (Cache policy).

use std::ops::RangeInclusive;

use crate::config::model::{CacheMode, LayerCacheConfig};

/// Resolve the effective cache mode for `level`, walking `level_rules` first
/// and falling back to `cache.mode` if none match.
///
/// Range parsing matches the validator's `parse_level_range` (`"18-21"` or
/// `"15"`); a malformed entry is ignored here (the validator is expected to
/// have already rejected it on config load).
pub fn resolve_mode(cache: &LayerCacheConfig, level: u32) -> CacheMode {
    for rule in &cache.level_rules {
        if let Some(range) = parse_level_range(&rule.levels) {
            if range.contains(&level) {
                return rule.mode;
            }
        }
    }
    cache.mode
}

fn parse_level_range(s: &str) -> Option<RangeInclusive<u32>> {
    let s = s.trim();
    if let Some((lo, hi)) = s.split_once('-') {
        let lo: u32 = lo.trim().parse().ok()?;
        let hi: u32 = hi.trim().parse().ok()?;
        if lo > hi {
            return None;
        }
        Some(lo..=hi)
    } else {
        let v: u32 = s.parse().ok()?;
        Some(v..=v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::model::{CacheMode, LayerCacheConfig, LevelRule};

    #[test]
    fn no_rules_returns_default_mode() {
        let cache = LayerCacheConfig::default();
        assert_eq!(resolve_mode(&cache, 0), CacheMode::Cache);
        assert_eq!(resolve_mode(&cache, 22), CacheMode::Cache);
    }

    #[test]
    fn rule_applies_inside_range() {
        let cache = LayerCacheConfig {
            mode: CacheMode::Cache,
            level_rules: vec![LevelRule {
                levels: "18-21".into(),
                mode: CacheMode::Passthrough,
            }],
            ..LayerCacheConfig::default()
        };
        assert_eq!(resolve_mode(&cache, 18), CacheMode::Passthrough);
        assert_eq!(resolve_mode(&cache, 20), CacheMode::Passthrough);
        assert_eq!(resolve_mode(&cache, 21), CacheMode::Passthrough);
        // outside the range -> default
        assert_eq!(resolve_mode(&cache, 5), CacheMode::Cache);
        assert_eq!(resolve_mode(&cache, 17), CacheMode::Cache);
        assert_eq!(resolve_mode(&cache, 22), CacheMode::Cache);
    }

    #[test]
    fn single_level_rule_parses() {
        let cache = LayerCacheConfig {
            mode: CacheMode::Cache,
            level_rules: vec![LevelRule {
                levels: "15".into(),
                mode: CacheMode::Passthrough,
            }],
            ..LayerCacheConfig::default()
        };
        assert_eq!(resolve_mode(&cache, 14), CacheMode::Cache);
        assert_eq!(resolve_mode(&cache, 15), CacheMode::Passthrough);
        assert_eq!(resolve_mode(&cache, 16), CacheMode::Cache);
    }

    #[test]
    fn first_matching_rule_wins() {
        let cache = LayerCacheConfig {
            mode: CacheMode::Cache,
            level_rules: vec![
                LevelRule { levels: "0-9".into(), mode: CacheMode::Passthrough },
                LevelRule { levels: "10-19".into(), mode: CacheMode::Cache },
            ],
            ..LayerCacheConfig::default()
        };
        assert_eq!(resolve_mode(&cache, 5), CacheMode::Passthrough);
        assert_eq!(resolve_mode(&cache, 15), CacheMode::Cache);
    }

    #[test]
    fn malformed_rule_is_ignored() {
        let cache = LayerCacheConfig {
            mode: CacheMode::Cache,
            level_rules: vec![LevelRule {
                levels: "garbage".into(),
                mode: CacheMode::Passthrough,
            }],
            ..LayerCacheConfig::default()
        };
        assert_eq!(resolve_mode(&cache, 10), CacheMode::Cache);
    }
}
