//! kwms — WM(T)S server for aerial imagery.
//!
//! Design documentation lives in `docs/` (entry point: `docs/src/SUMMARY.md`).
//! This crate exposes its internals as a library so integration tests under
//! `tests/` can drive the server in-process. The binary entry point is
//! `src/main.rs`.

// Internet-facing: integer arithmetic must not overflow or panic silently.
// Every `+ - * / % <<` on integers in non-test code must be checked,
// saturating or wrapping *explicitly* (floating point is not affected).
// Release builds additionally keep `overflow-checks = true` (Cargo.toml).
#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]
// `unsafe` is denied everywhere except `ffi` (see that module and
// docs/src/unsafe-audit.md). Inside `ffi`, each unsafe operation gets its own
// block with a SAFETY comment; tests/unsafe_audit.rs ties every block to its
// documentation entry.
#![deny(unsafe_code)]
#![deny(clippy::undocumented_unsafe_blocks, clippy::multiple_unsafe_ops_per_block)]

pub mod cache;
// The HTTP server (and the CLI that starts it) is left out of fuzzing builds:
// the fuzz targets only drive `protocol`, and nightly rustc (2026-11-17)
// overflows proving the axum handler futures `Send` in release mode — stable
// builds the same code fine. See fuzz/README.md.
#[cfg(not(fuzzing))]
pub mod cli;
pub mod config;
pub mod crs;
pub mod encode;
#[allow(unsafe_code)]
pub(crate) mod ffi;
pub mod pipeline;
pub mod protocol;
#[cfg(not(fuzzing))]
pub mod server;
pub mod source;
#[cfg(not(fuzzing))]
pub mod subcommand;
pub mod tile;
