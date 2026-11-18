//! Tile cache — two-tier (in-process + persistent) with per-layer policy.
//!
//! See `docs/src/caching.md`.

pub mod hot;
pub mod key;
pub mod layered;
pub mod persistent;
pub mod policy;

pub use self::key::TileKey;
pub use self::layered::{Computed, LayeredCache};
