//! Configuration types, loading, merging, validation, schema export.
//!
//! See `docs/src/configuration.md`.

pub mod load;
pub mod merge;
pub mod model;
pub mod schema;
pub mod validate;

pub use self::model::*;
