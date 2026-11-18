//! Protocol request parsing, validation and response shaping. No I/O.
//!
//! Requests pass through two explicit stages before anything else sees them:
//! [`kvp::Kvp`] (strict query-string decoding, shared by WMS and WMTS) and
//! per-protocol parsing/validation into typed requests. Request errors are
//! [`error::OgcError`]s.

pub mod catalog;
pub mod error;
#[cfg(any(test, fuzzing))]
pub mod fuzz_harness;
pub mod kvp;
pub mod query;
pub mod tms;
pub mod wms;
pub mod wmts;
