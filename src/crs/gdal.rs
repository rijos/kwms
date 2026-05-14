//! GDAL/PROJ-backed CRS operations.
//!
//! This file is one of three places allowed to depend on the `gdal` crate
//! (the others being `source/gdal/` and `encode.rs`). Driver registration,
//! PROJ initialization, and error mapping live here.

use anyhow::Result;

use super::{AxisOrder, Bbox, Crs};

pub fn is_geographic(_crs: &Crs) -> Result<bool> {
    todo!("phase 2 (crs agent)")
}

pub fn native_axis_order(_crs: &Crs) -> Result<AxisOrder> {
    todo!("phase 2 (crs agent)")
}

pub fn transform_bbox(_bbox: &Bbox, _from: &Crs, _to: &Crs) -> Result<Bbox> {
    todo!("phase 2 (crs agent)")
}

pub fn validate(_crs: &Crs) -> Result<()> {
    todo!("phase 2 (crs agent)")
}
