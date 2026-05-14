//! GDAL-backed [`Source`] implementation.
//!
//! One of three modules permitted to call into the `gdal` crate directly
//! (alongside `crs/gdal.rs` and `encode.rs`).

pub mod dataset;
pub mod pool;
pub mod sample;
pub mod warp;

use crate::config::SourceConfig;
use crate::crs::{Coord, Crs};

use super::{PixelBuffer, PixelValue, Source, SourceMetadata, WindowRequest};

pub struct GdalSource {
    // To be filled by the source-gdal agent: dataset pool, cached metadata, etc.
}

impl GdalSource {
    pub fn open(_config: &SourceConfig) -> anyhow::Result<Self> {
        todo!("phase 2 (source-gdal agent)")
    }
}

#[async_trait::async_trait]
impl Source for GdalSource {
    fn metadata(&self) -> &SourceMetadata {
        todo!("phase 2 (source-gdal agent)")
    }

    async fn read_window(&self, _req: WindowRequest) -> anyhow::Result<PixelBuffer> {
        todo!("phase 2 (source-gdal agent)")
    }

    async fn sample(&self, _coord: Coord, _crs: Crs) -> anyhow::Result<Vec<PixelValue>> {
        todo!("phase 2 (source-gdal agent)")
    }
}
