//! WMTS 1.0.0: `request` (stage 1, syntax) → `validate` (stage 2, rules
//! against the catalog) → [`crate::protocol::query::TileQuery`].

pub mod capabilities;
pub mod exception;
pub mod request;
pub mod validate;
