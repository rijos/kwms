//! GDAL/PROJ-backed CRS operations.
//!
//! This file is one of three places allowed to depend on the `gdal` crate
//! (the others being `source/gdal/` and `encode.rs`). Driver registration,
//! PROJ initialization, and error mapping live here.
//!
//! Notes on axis order: with PROJ >= 6 / GDAL >= 3 the default axis-mapping
//! strategy is `AUTHORITY_COMPLIANT`, so e.g. `EPSG:4326` reports its first
//! axis as North (latitude). `native_axis_order` exposes that raw view.
//! For [`transform_bbox`] we always force `TraditionalGisOrder` on both
//! source and target so the `Bbox` (`minx`, `miny`, `maxx`, `maxy`) numeric
//! convention is interpreted XY end-to-end — otherwise the same four numbers
//! would mean different things depending on the CRS's authority axis order.

use anyhow::{Context, Result};

use gdal::spatial_ref::{
    AxisMappingStrategy, AxisOrientationType, CoordTransform, SpatialRef,
};

use super::{AxisOrder, Bbox, Crs};

// `AxisOrientationType` is a C-style integer alias (`gdal_sys::OGRAxisOrientation::Type`).
// We don't depend on `gdal-sys` directly, so we mirror the few enum values
// we care about. These values are stable across GDAL versions (defined in
// `ogr_core.h` / `OGRAxisOrientation`).
const OAO_NORTH: AxisOrientationType = 1;
const OAO_SOUTH: AxisOrientationType = 2;
const OAO_EAST: AxisOrientationType = 3;
const OAO_WEST: AxisOrientationType = 4;

/// Build a `SpatialRef` from an opaque `Crs` token. Accepts anything
/// `OSRSetFromUserInput` understands (EPSG codes, `AUTH:CODE`, WKT, PROJ.4,
/// `urn:ogc:def:crs:...`, ...).
fn spatial_ref(crs: &Crs) -> Result<SpatialRef> {
    SpatialRef::from_definition(crs.as_str())
        .with_context(|| format!("invalid or unknown CRS '{}'", crs.as_str()))
}

pub fn is_geographic(crs: &Crs) -> Result<bool> {
    Ok(spatial_ref(crs)?.is_geographic())
}

pub fn native_axis_order(crs: &Crs) -> Result<AxisOrder> {
    let sr = spatial_ref(crs)?;
    // GDAL's `OSRGetAxis` requires the WKT node key under which the axes
    // live: "PROJCS" for projected, "GEOGCS" for geographic. (An empty key
    // is rejected with "Axis not found".) Pick the right one based on the
    // CRS classification, falling back through both for unusual cases.
    let candidates: &[&str] = if sr.is_projected() {
        &["PROJCS", "GEOGCS"]
    } else if sr.is_geographic() {
        &["GEOGCS", "PROJCS"]
    } else {
        &["PROJCS", "GEOGCS", "GEOCCS"]
    };

    let mut last_err: Option<anyhow::Error> = None;
    for key in candidates {
        match sr.axis_orientation(key, 0) {
            Ok(o) => return Ok(axis_order_from_first(o)),
            Err(e) => last_err = Some(e.into()),
        }
    }

    Err(last_err
        .unwrap_or_else(|| anyhow::anyhow!("no axes reported"))
        .context(format!("could not read axis 0 of CRS '{}'", crs.as_str())))
}

fn axis_order_from_first(o: AxisOrientationType) -> AxisOrder {
    match o {
        OAO_EAST | OAO_WEST => AxisOrder::XY,
        OAO_NORTH | OAO_SOUTH => AxisOrder::YX,
        // OAO_Up / OAO_Down / OAO_Other are not meaningful as a horizontal
        // first axis; fall back to XY (the WMS-1.1.1-style assumption) so we
        // don't panic on exotic compound CRSes.
        _ => AxisOrder::XY,
    }
}

pub fn transform_bbox(bbox: &Bbox, from: &Crs, to: &Crs) -> Result<Bbox> {
    let mut src = spatial_ref(from)?;
    let mut dst = spatial_ref(to)?;
    // Force traditional GIS (lon, lat) ordering on both ends so the
    // numeric (minx, miny) / (maxx, maxy) inputs are always interpreted as
    // (east, north) regardless of the CRS's authority axis order.
    src.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    dst.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);

    let ct = CoordTransform::new(&src, &dst).with_context(|| {
        format!(
            "could not build coordinate transform from '{}' to '{}'",
            from.as_str(),
            to.as_str()
        )
    })?;

    // Transform the four corners of the bbox and take the axis-aligned hull
    // of the results. For non-linear transforms this under-covers along the
    // edges, but the spec explicitly asks for the four-corner approach; the
    // pipeline is expected to densify when it needs tight bounds.
    let mut xs = [bbox.minx, bbox.maxx, bbox.maxx, bbox.minx];
    let mut ys = [bbox.miny, bbox.miny, bbox.maxy, bbox.maxy];
    let mut zs: [f64; 0] = [];

    ct.transform_coords(&mut xs, &mut ys, &mut zs)
        .with_context(|| {
            format!(
                "failed to transform bbox from '{}' to '{}'",
                from.as_str(),
                to.as_str()
            )
        })?;

    let minx = xs.iter().cloned().fold(f64::INFINITY, f64::min);
    let maxx = xs.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
    let miny = ys.iter().cloned().fold(f64::INFINITY, f64::min);
    let maxy = ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    if !(minx.is_finite() && maxx.is_finite() && miny.is_finite() && maxy.is_finite()) {
        anyhow::bail!(
            "bbox transform from '{}' to '{}' produced non-finite coordinates",
            from.as_str(),
            to.as_str()
        );
    }

    Ok(Bbox { minx, miny, maxx, maxy })
}

pub fn validate(crs: &Crs) -> Result<()> {
    spatial_ref(crs).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn epsg(code: u32) -> Crs {
        Crs::new(format!("EPSG:{code}"))
    }

    #[test]
    fn is_geographic_wgs84() {
        assert!(is_geographic(&epsg(4326)).unwrap());
    }

    #[test]
    fn is_geographic_webmercator() {
        assert!(!is_geographic(&epsg(3857)).unwrap());
    }

    #[test]
    fn native_axis_order_is_defined_for_4326_and_3857() {
        // We don't pin the exact value here because PROJ versions vary in
        // how they report EPSG:4326 (modern PROJ: YX / authority-compliant;
        // ancient builds or some configurations: XY). We only assert that
        // the call succeeds and returns one of the two variants.
        let g = native_axis_order(&epsg(4326)).unwrap();
        assert!(matches!(g, AxisOrder::XY | AxisOrder::YX));

        // EPSG:3857 (Web Mercator) is projected; first axis is Easting.
        let p = native_axis_order(&epsg(3857)).unwrap();
        assert_eq!(p, AxisOrder::XY);
    }

    #[test]
    fn transform_bbox_4326_to_3857_is_plausible() {
        let src = Bbox {
            minx: 3.0,
            miny: 50.0,
            maxx: 7.0,
            maxy: 54.0,
        };
        let out = transform_bbox(&src, &epsg(4326), &epsg(3857)).unwrap();

        assert!(out.maxx > out.minx, "maxx > minx, got {out:?}");
        assert!(out.maxy > out.miny, "maxy > miny, got {out:?}");

        // Web-Mercator easting of 3°E ≈ 333_958 m; of 7°E ≈ 779_236 m.
        // Web-Mercator northing of 50°N ≈ 6_446_276 m; of 54°N ≈ 7_170_157 m.
        // Allow generous tolerance — what we really want to assert is order
        // of magnitude and orientation.
        assert!(
            (200_000.0..500_000.0).contains(&out.minx),
            "minx out of plausible range: {}",
            out.minx
        );
        assert!(
            (600_000.0..900_000.0).contains(&out.maxx),
            "maxx out of plausible range: {}",
            out.maxx
        );
        assert!(
            (6_000_000.0..7_000_000.0).contains(&out.miny),
            "miny out of plausible range: {}",
            out.miny
        );
        assert!(
            (6_800_000.0..7_500_000.0).contains(&out.maxy),
            "maxy out of plausible range: {}",
            out.maxy
        );
    }

    #[test]
    fn validate_known_crs_ok() {
        validate(&epsg(4326)).unwrap();
        validate(&epsg(3857)).unwrap();
    }

    #[test]
    fn validate_bogus_crs_err() {
        let err = validate(&Crs::new("EPSG:99999")).unwrap_err();
        // Error message should mention the offending CRS string so operators
        // can find it in logs.
        let msg = format!("{err:#}");
        assert!(
            msg.contains("99999"),
            "expected error to mention the bad CRS, got: {msg}"
        );
    }

    #[test]
    fn validate_garbage_string_err() {
        assert!(validate(&Crs::new("not-a-crs-at-all")).is_err());
    }
}
