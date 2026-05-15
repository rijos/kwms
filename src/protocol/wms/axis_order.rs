//! WMS axis-order rules.
//!
//! WMS 1.1.1 historically interpreted every BBOX as `minx,miny,maxx,maxy`
//! (XY), regardless of CRS. WMS 1.3.0 honors each CRS's *authority* axis
//! order — so geographic CRSes like `EPSG:4326` use `lat,lon` (YX) on the
//! wire.
//!
//! This module exposes a single predicate, [`should_swap_axes`], that the
//! request parser and capabilities writer consult to decide whether the
//! wire-format axis order differs from kwms's internal XY convention
//! (which matches the numeric layout of [`crate::crs::Bbox`]).

use anyhow::Result;

use crate::crs::{is_geographic, native_axis_order, AxisOrder, Crs};

use super::request::WmsVersion;

/// Returns `true` if the WMS wire format for `crs` under `version` lists
/// coordinates in YX order (and therefore needs to be swapped to populate
/// kwms's XY-internal [`crate::crs::Bbox`]).
///
/// - WMS 1.1.1 → always `false` (the spec mandates `minx,miny,maxx,maxy`).
/// - WMS 1.3.0 → `true` iff the CRS is geographic *and* its authority axis
///   order is YX. Projected CRSes are XY in both versions.
pub fn should_swap_axes(crs: &Crs, version: WmsVersion) -> Result<bool> {
    match version {
        WmsVersion::V1_1_1 => Ok(false),
        WmsVersion::V1_3_0 => {
            if !is_geographic(crs)? {
                return Ok(false);
            }
            Ok(matches!(native_axis_order(crs)?, AxisOrder::YX))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epsg(code: u32) -> Crs {
        Crs::new(format!("EPSG:{code}"))
    }

    #[test]
    fn matrix_4326_and_3857_under_both_versions() {
        // EPSG:4326 under 1.1.1 → never swap.
        assert!(!should_swap_axes(&epsg(4326), WmsVersion::V1_1_1).unwrap());

        // EPSG:4326 under 1.3.0 → swap iff PROJ reports YX (modern PROJ does).
        // We assert the spec-correct rule conditional on what `native_axis_order`
        // reports, so this test is robust on hosts where the PROJ build pins
        // 4326 to XY (the "swap" expectation only applies when YX).
        let native = native_axis_order(&epsg(4326)).unwrap();
        let v130 = should_swap_axes(&epsg(4326), WmsVersion::V1_3_0).unwrap();
        assert_eq!(v130, native == AxisOrder::YX);

        // EPSG:3857 is projected → never swap under either version.
        assert!(!should_swap_axes(&epsg(3857), WmsVersion::V1_1_1).unwrap());
        assert!(!should_swap_axes(&epsg(3857), WmsVersion::V1_3_0).unwrap());
    }
}
