//! Where request time goes: wall-clock seconds per pipeline stage, as the
//! `kwms_stage_seconds{stage}` summary (`_sum` / `_count` give totals).
//!
//! | stage            | what                                                        |
//! |------------------|-------------------------------------------------------------|
//! | `gdal_wait`      | waiting for a free dataset (per-source pool)                 |
//! | `gdal_run`       | the GDAL job: source read + warp (network I/O + decode)       |
//! | `cpu_wait`       | waiting for a render slot                                    |
//! | `encode`         | encoding a tile (CPU)                                        |
//! | `wms_tiles`      | WMS: fetching/rendering the covering tiles                   |
//! | `wms_composite`  | WMS: decoding, mosaicking and encoding the image (CPU)        |
//!
//! Cache tier I/O is `kwms_cache_seconds{tier, op}`. `kwms-bench`
//! prints both per run.

use std::time::Instant;

/// Record the time since `started` for `stage`.
pub fn record(stage: &'static str, started: Instant) {
    metrics::histogram!("kwms_stage_seconds", "stage" => stage).record(started.elapsed().as_secs_f64());
}
