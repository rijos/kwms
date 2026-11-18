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

use anyhow::{bail, Context, Result};

use gdal::spatial_ref::{AxisMappingStrategy, CoordTransform, SpatialRef};

use super::{AxisOrder, Bbox, Coord, Crs};
use crate::ffi;

/// Build a `SpatialRef` from an opaque `Crs` token. Accepts anything
/// `OSRSetFromUserInput` understands (EPSG codes, `AUTH:CODE`, WKT, PROJ.4,
/// `urn:ogc:def:crs:...`, ...) — including file paths and URLs, which GDAL
/// will read/fetch. Only ever call this with config-derived CRSes: request
/// CRSes are resolved by `protocol::catalog`, never parsed here.
fn spatial_ref(crs: &Crs) -> Result<SpatialRef> {
    SpatialRef::from_definition(crs.as_str())
        .with_context(|| format!("invalid or unknown CRS '{}'", crs.as_str()))
}

pub fn is_geographic(crs: &Crs) -> Result<bool> {
    Ok(spatial_ref(crs)?.is_geographic())
}

/// The CRS's authority axis order, as GDAL sees it.
///
/// Asks GDAL rather than inspecting axes ourselves: with the traditional GIS
/// mapping strategy, GDAL maps data axes to CRS axes as `[2, 1]` exactly
/// when the authority order is northing/latitude first (EPSG:4326, but also
/// projected CRSes such as EPSG:3035).
pub fn native_axis_order(crs: &Crs) -> Result<AxisOrder> {
    let mut sr = spatial_ref(crs)?;
    sr.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    let mapping = ffi::data_axis_mapping(&sr).with_context(|| format!("axis mapping of CRS '{crs}'"))?;
    // Exactly the two mappings a 2D CRS can have under the traditional GIS
    // strategy; anything else (e.g. inverted axes) is an error, never a guess.
    match mapping.get(..2) {
        Some([1, 2]) => Ok(AxisOrder::XY),
        Some([2, 1]) => Ok(AxisOrder::YX),
        _ => bail!("unexpected axis mapping {mapping:?} for CRS '{crs}'"),
    }
}

/// Whether two CRS tokens denote the same CRS, per GDAL (`OSRIsSame`, which
/// ignores geographic axis order — so `CRS:84` ≡ `EPSG:4326`). A token GDAL
/// can't parse is an error, not "different".
pub fn same_crs(a: &Crs, b: &Crs) -> Result<bool> {
    if a == b {
        return Ok(true);
    }
    Ok(spatial_ref(a)? == spatial_ref(b)?)
}

/// Canonical `AUTH:CODE` for a CRS definition (e.g. ESRI WKT for RD New →
/// `EPSG:28992`), using GDAL's `OSRFindMatches` (via `crate::ffi`).
///
/// * `Ok` with the input unchanged when GDAL finds no exact (100%) match —
///   that is an answer, not a failure;
/// * `Err` when GDAL itself fails (unparseable definition, or an error
///   raised while matching). Never a silent fallback on failure.
pub fn identify_crs(crs: &Crs) -> Result<Crs> {
    let sr = spatial_ref(crs)?;
    if let (Ok(name), Ok(code)) = (sr.auth_name(), sr.auth_code()) {
        if !name.is_empty() {
            return Ok(Crs::new(format!("{name}:{code}")));
        }
    }
    for (candidate, confidence) in ffi::find_matches(&sr).with_context(|| format!("matching CRS '{crs}'"))? {
        if confidence < 100 {
            continue;
        }
        if let (Ok(name), Ok(code)) = (candidate.auth_name(), candidate.auth_code()) {
            return Ok(Crs::new(format!("{name}:{code}")));
        }
    }
    Ok(crs.clone())
}

/// Metres per CRS unit, for OGC scale denominators. Projected CRSes report
/// their linear unit; geographic ones use the length of one unit of arc on
/// the ellipsoid's equator (the convention in OGC WMTS Annex E).
pub fn meters_per_unit(crs: &Crs) -> Result<f64> {
    let sr = spatial_ref(crs)?;
    if sr.is_geographic() {
        let a = sr.semi_major().context("CRS has no semi-major axis")?;
        Ok(a * sr.angular_units())
    } else {
        Ok(sr.linear_units())
    }
}

/// Points sampled per bbox edge by [`transform_bbox`]. GDAL's recommended
/// default for `OCTTransformBounds` (and what `gdalwarp`/rasterio use).
const DENSIFY_POINTS: i32 = 21;

/// Build a transform that reads and writes coordinates in traditional GIS
/// (x = easting/longitude, y = northing/latitude) order, regardless of the
/// CRS's authority axis order. Every transform in kwms goes through here so
/// the `Bbox`/`Coord` numeric convention means the same thing end-to-end.
fn gis_order_transform(from: &Crs, to: &Crs) -> Result<CoordTransform> {
    let mut src = spatial_ref(from)?;
    let mut dst = spatial_ref(to)?;
    src.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    dst.set_axis_mapping_strategy(AxisMappingStrategy::TraditionalGisOrder);
    CoordTransform::new(&src, &dst)
        .with_context(|| format!("could not build coordinate transform from '{from}' to '{to}'"))
}

/// Reproject a bbox, delegating to GDAL's `OCTTransformBounds`: it densifies
/// every edge (so curved edges aren't under-covered), and copes with poles
/// and points outside the target projection's domain.
///
/// When the target CRS is geographic and the box crosses the antimeridian,
/// GDAL signals it with `minx > maxx`. `Bbox` can't represent a wrapped box,
/// so we widen x to the full −180..180 range: a superset, never an
/// under-cover.
pub fn transform_bbox(bbox: &Bbox, from: &Crs, to: &Crs) -> Result<Bbox> {
    let ct = gis_order_transform(from, to)?;
    let [minx, miny, maxx, maxy] = ct
        .transform_bounds(&[bbox.minx, bbox.miny, bbox.maxx, bbox.maxy], DENSIFY_POINTS)
        .with_context(|| format!("failed to transform bbox from '{from}' to '{to}'"))?;
    if ![minx, miny, maxx, maxy].iter().all(|v| v.is_finite()) {
        anyhow::bail!("bbox transform from '{from}' to '{to}' produced non-finite coordinates");
    }
    let (minx, maxx) = if minx > maxx { (-180.0, 180.0) } else { (minx, maxx) };
    Ok(Bbox { minx, miny, maxx, maxy })
}

/// Reproject a single coordinate.
pub fn transform_coord(coord: Coord, from: &Crs, to: &Crs) -> Result<Coord> {
    let ct = gis_order_transform(from, to)?;
    let (mut xs, mut ys, mut zs) = ([coord.x], [coord.y], [0.0f64; 0]);
    ct.transform_coords(&mut xs, &mut ys, &mut zs)
        .with_context(|| format!("failed to transform coordinate from '{from}' to '{to}'"))?;
    if !(xs[0].is_finite() && ys[0].is_finite()) {
        anyhow::bail!("coordinate transform from '{from}' to '{to}' produced non-finite values");
    }
    Ok(Coord { x: xs[0], y: ys[0] })
}

/// Forbid PROJ from fetching transformation grids over the network, for
/// the whole process. With network access on (`PROJ_NETWORK=ON` or
/// `network = on` in `proj.ini`), a coordinate transform inside a request
/// could block on an outbound HTTP download. kwms uses only locally
/// installed grids. Call once at startup, before any transform; errors if
/// GDAL doesn't report the setting as applied.
pub fn disable_proj_network() -> Result<()> {
    ffi::disable_proj_network();
    if ffi::proj_network_enabled() {
        bail!("could not disable PROJ network access");
    }
    Ok(())
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
    fn proj_network_access_is_disabled() {
        disable_proj_network().unwrap();
        assert!(!ffi::proj_network_enabled());
        // Idempotent: a second call (e.g. a second AppState) is fine.
        disable_proj_network().unwrap();
        assert!(!ffi::proj_network_enabled());
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
    fn transform_bbox_covers_curved_edges() {
        // LAEA's northern edge bulges far past its corners in lon/lat: the
        // corners alone reach ~66.99°N, the true edge ~72.66°N.
        let laea = Bbox::from([2_500_000.0, 1_400_000.0, 6_500_000.0, 5_500_000.0]);
        let out = transform_bbox(&laea, &epsg(3035), &epsg(4326)).unwrap();
        assert!(out.maxy > 72.5, "northern edge under-covered: {out:?}");
    }

    #[test]
    fn transform_bbox_whole_world_to_webmercator_is_finite() {
        let world = Bbox::from([-180.0, -90.0, 180.0, 90.0]);
        let out = transform_bbox(&world, &epsg(4326), &epsg(3857)).unwrap();
        assert!(out.minx <= -20_037_508.0 && out.maxx >= 20_037_508.0, "{out:?}");
    }

    #[test]
    fn transform_bbox_antimeridian_widens_to_full_longitude() {
        // EPSG:3832 is centred on 150°E; this box spans ~105°E..195°E.
        let pacific = Bbox::from([-5_000_000.0, -3_000_000.0, 5_000_000.0, 3_000_000.0]);
        let out = transform_bbox(&pacific, &epsg(3832), &epsg(4326)).unwrap();
        assert_eq!((out.minx, out.maxx), (-180.0, 180.0), "{out:?}");
        assert!(out.miny < -26.0 && out.maxy > 26.0, "{out:?}");
    }

    #[test]
    fn transform_coord_amsterdam_to_webmercator() {
        let c = transform_coord(Coord { x: 4.9, y: 52.37 }, &epsg(4326), &epsg(3857)).unwrap();
        assert!((c.x - 545_465.5).abs() < 1.0 && (c.y - 6_867_304.7).abs() < 1.0, "{c:?}");
    }

    #[test]
    fn axis_order_comes_from_gdal_for_geographic_and_projected() {
        assert_eq!(native_axis_order(&epsg(4326)).unwrap(), AxisOrder::YX);
        assert_eq!(native_axis_order(&epsg(3035)).unwrap(), AxisOrder::YX, "LAEA is northing-first");
        assert_eq!(native_axis_order(&epsg(3857)).unwrap(), AxisOrder::XY);
        assert_eq!(native_axis_order(&epsg(28992)).unwrap(), AxisOrder::XY);
        assert_eq!(native_axis_order(&Crs::new("CRS:84")).unwrap(), AxisOrder::XY);
        assert!(native_axis_order(&Crs::new("not-a-crs")).is_err());
    }

    #[test]
    fn same_crs_uses_gdal_identity() {
        assert!(same_crs(&epsg(4326), &Crs::new("CRS:84")).unwrap());
        assert!(!same_crs(&epsg(4326), &epsg(3857)).unwrap());
        assert!(same_crs(&Crs::new("garbage"), &epsg(3857)).is_err(), "parse failure is an error");
    }

    #[test]
    fn identify_crs_maps_esri_wkt_to_epsg() {
        let sr = SpatialRef::from_epsg(28992).unwrap();
        sr.morph_to_esri().unwrap();
        let esri = Crs::new(sr.to_wkt().unwrap());
        assert_eq!(identify_crs(&esri).unwrap(), epsg(28992));
        assert_eq!(identify_crs(&epsg(3857)).unwrap(), epsg(3857));
        assert!(identify_crs(&Crs::new("not-a-crs")).is_err(), "GDAL failure is an error, not a fallback");
    }

    #[test]
    fn meters_per_unit_for_degrees_and_metres() {
        assert!((meters_per_unit(&epsg(4326)).unwrap() - 111_319.490_793).abs() < 1e-3);
        assert_eq!(meters_per_unit(&epsg(3857)).unwrap(), 1.0);
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
