//! Tile cache — a configurable stack of tiers ([`CacheTier`]: in-process
//! memory, opendal stores) behind one single-flight front ([`LayeredCache`]),
//! with per-layer policy.
//!
//! See `docs/src/caching.md`.

pub mod key;
pub mod layered;
pub mod memory;
pub mod policy;
pub mod store;
pub mod tier;

pub use self::key::{grid_dir, TileKey};
pub use self::layered::{Computed, LayeredCache, Scope};
pub use self::tier::{CacheTier, Entry};
