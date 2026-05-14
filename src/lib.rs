//! kwms — WM(T)S server for aerial imagery.
//!
//! Design documentation lives in `docs/` (entry point: `docs/src/SUMMARY.md`).
//! This crate exposes its internals as a library so integration tests under
//! `tests/` can drive the server in-process. The binary entry point is
//! `src/main.rs`.

pub mod cache;
pub mod cli;
pub mod config;
pub mod crs;
pub mod encode;
pub mod pipeline;
pub mod protocol;
pub mod server;
pub mod source;
pub mod subcommand;
pub mod tile;
