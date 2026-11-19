//! Where an image has data: polygon areas rasterized into an alpha plane.
//!
//! A pixel has data when it lies inside every area that applies — the
//! source's coverage (a tile index's tile footprints, or a raster's extent)
//! and the layer's `source_mask` — and is not the nodata value. Everything
//! else is no data: transparent in PNG/WebP/GeoTIFF, white in JPEG (see
//! `encode`). Using footprints rather than "pixel value 0" keeps genuinely
//! black pixels (deep shadow) visible.
//!
//! Areas are stored as WKB and reprojected once per CRS by the caller; the
//! rasterization runs per image on the render pool, at the image's own
//! size, bbox and CRS (about a millisecond for the 7865-vertex NL mask).
//!
//! One of the modules permitted to call into the `gdal` crate directly.

use std::path::Path;
use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use bytes::Bytes;
use gdal::raster::{rasterize, RasterizeOptions};
use gdal::vector::{Geometry, LayerAccess, OGRwkbGeometryType};
use gdal::{Dataset, DatasetOptions, GdalOpenFlags};

use super::raster::{bbox_to_geotransform, create_mem, gis_order_spatial_ref, pixel_type_bytes};
use crate::crs::{identify_crs, Bbox, Crs};
use crate::source::{Area, PixelBuffer, PixelType};

/// Every polygon of every feature of every layer of the vector dataset at
/// `path`, as one area in the dataset's CRS.
pub fn load(path: &str) -> Result<Area> {
    let options = DatasetOptions {
        open_flags: GdalOpenFlags::GDAL_OF_VECTOR | GdalOpenFlags::GDAL_OF_READONLY,
        ..Default::default()
    };
    let ds = Dataset::open_ex(Path::new(path), options).with_context(|| format!("opening mask '{path}'"))?;
    let mut all = Geometry::empty(OGRwkbGeometryType::wkbGeometryCollection)?;
    let mut crs: Option<Crs> = None;
    let mut polygons = 0usize;
    for mut layer in ds.layers() {
        let srs = layer
            .spatial_ref()
            .ok_or_else(|| anyhow!("mask '{path}' layer '{}' has no CRS", layer.name()))?;
        let layer_crs = identify_crs(&Crs::new(srs.to_wkt()?))?;
        match &crs {
            Some(c) if *c != layer_crs => bail!("mask '{path}': layers in different CRSes ({c}, {layer_crs})"),
            _ => crs = Some(layer_crs),
        }
        for feature in layer.features() {
            let Some(g) = feature.geometry() else { continue };
            let kind = g.geometry_type();
            if kind != OGRwkbGeometryType::wkbPolygon && kind != OGRwkbGeometryType::wkbMultiPolygon {
                bail!(
                    "mask '{path}' has a {} feature; only polygons are supported",
                    g.geometry_name()
                );
            }
            all.add_geometry(g.clone())?;
            polygons = polygons.saturating_add(1);
        }
    }
    let crs = crs.ok_or_else(|| anyhow!("mask '{path}' has no layers"))?;
    if polygons == 0 {
        bail!("mask '{path}' has no polygons");
    }
    Ok(Area {
        wkb: Arc::new(all.wkb()?),
        crs,
        trim_edges: false,
    })
}

/// A rectangle as an area (a raster's extent: edges trimmed).
pub fn rectangle(b: &Bbox, crs: &Crs) -> Result<Area> {
    let wkt = format!(
        "POLYGON (({x0} {y0}, {x1} {y0}, {x1} {y1}, {x0} {y1}, {x0} {y0}))",
        x0 = b.minx,
        y0 = b.miny,
        x1 = b.maxx,
        y1 = b.maxy
    );
    Ok(Area {
        wkb: Arc::new(Geometry::from_wkt(&wkt)?.wkb()?),
        crs: crs.clone(),
        trim_edges: true,
    })
}

/// The footprints of a tile index dissolved into one outline (edges
/// trimmed). Dissolving matters: only the *outer* edge of the coverage is
/// trimmed, never the seams between neighbouring tiles. Unioned pairwise
/// (a balanced tree), so 500 tiles take a fraction of a second.
///
/// Coordinates are first snapped to a grid of `snap` CRS units: index
/// footprints that should touch often miss by float noise (the 2026 index
/// has edges at 480000 and 480000.000000051), and the union would keep that
/// sliver as an inner edge — trimmed as a no-data line through imagery.
pub fn dissolve(polygons: &[Vec<u8>], crs: &Crs, snap: f64) -> Result<Area> {
    if !(snap > 0.0 && snap.is_finite()) {
        bail!("invalid snap grid {snap}");
    }
    let mut level: Vec<Geometry> = polygons
        .iter()
        .map(|w| {
            Geometry::from_wkb(w)
                .map_err(anyhow::Error::from)
                .and_then(|g| snap_to_grid(&g, snap))
        })
        .collect::<Result<_>>()?;
    if level.is_empty() {
        bail!("no footprints to dissolve");
    }
    while level.len() > 1 {
        let mut next = Vec::with_capacity(level.len().div_ceil(2));
        let mut it = level.into_iter();
        while let Some(a) = it.next() {
            next.push(match it.next() {
                Some(b) => a
                    .union(&b)
                    .ok_or_else(|| anyhow!("dissolving tile footprints failed (GEOS)"))?,
                None => a,
            });
        }
        level = next;
    }
    let outline = level.pop().ok_or_else(|| anyhow!("no footprints"))?;
    Ok(Area {
        wkb: Arc::new(outline.wkb()?),
        crs: crs.clone(),
        trim_edges: true,
    })
}

/// `area` in `to` (a clone if already there).
pub fn reproject(area: &Area, to: &Crs) -> Result<Area> {
    if area.crs == *to {
        return Ok(area.clone());
    }
    let mut g = Geometry::from_wkb(&area.wkb)?;
    g.set_spatial_ref(gis_order_spatial_ref(&area.crs)?);
    let moved = g
        .transform_to(&gis_order_spatial_ref(to)?)
        .with_context(|| format!("reprojecting an area from {} to {to}", area.crs))?;
    Ok(Area {
        wkb: Arc::new(moved.wkb()?),
        crs: to.clone(),
        trim_edges: area.trim_edges,
    })
}

/// One byte per pixel of a `width` x `height` image of `bbox`: 255 where
/// every area (already in the image's CRS) covers the pixel centre, else 0.
pub fn coverage(areas: &[Area], bbox: &Bbox, width: u32, height: u32) -> Result<Vec<u8>> {
    let n = (width as usize)
        .checked_mul(height as usize)
        .context("image size overflows")?;
    let mut out = vec![255u8; n];
    for area in areas {
        let mut ds = create_mem(PixelType::U8, width, height, 1)?;
        ds.set_geo_transform(&bbox_to_geotransform(bbox, width, height)?)?;
        let geometry = Geometry::from_wkb(&area.wkb)?;
        let outline = if area.trim_edges { rings(&geometry)? } else { Vec::new() };
        rasterize(&mut ds, &[1], &[geometry], &[255.0], None).context("rasterizing a coverage area")?;
        if !outline.is_empty() {
            let zeros = vec![0.0; outline.len()];
            let touched = RasterizeOptions {
                all_touched: true,
                ..Default::default()
            };
            rasterize(&mut ds, &[1], &outline, &zeros, Some(touched)).context("trimming the coverage outline")?;
        }
        let band = ds.rasterband(1)?.read_band_as::<u8>()?;
        let (_, burned) = band.into_shape_and_vec();
        for (o, b) in out.iter_mut().zip(burned) {
            *o = (*o).min(b);
        }
    }
    Ok(out)
}

/// `g` (polygons / multipolygons) with every vertex rounded to `grid`.
fn snap_to_grid(g: &Geometry, grid: f64) -> Result<Geometry> {
    let kind = g.geometry_type();
    let round = |v: f64| (v / grid).round() * grid;
    // A polygon's rings report the LineString type (only their name says
    // LINEARRING).
    if kind == OGRwkbGeometryType::wkbLinearRing || kind == OGRwkbGeometryType::wkbLineString {
        let mut ring = Geometry::empty(OGRwkbGeometryType::wkbLinearRing)?;
        for (x, y, _) in g.get_point_vec() {
            ring.add_point_2d((round(x), round(y)));
        }
        return Ok(ring);
    }
    let out_kind = match kind {
        k if k == OGRwkbGeometryType::wkbPolygon => OGRwkbGeometryType::wkbPolygon,
        k if k == OGRwkbGeometryType::wkbMultiPolygon => OGRwkbGeometryType::wkbMultiPolygon,
        _ => bail!("tile footprint is a {}; only polygons are supported", g.geometry_name()),
    };
    let mut out = Geometry::empty(out_kind)?;
    for i in 0..g.geometry_count() {
        out.add_geometry(snap_to_grid(&g.get_geometry(i), grid)?)?;
    }
    Ok(out)
}

/// Every ring of `g` (polygon shells and holes, recursively) as a line.
fn rings(g: &Geometry) -> Result<Vec<Geometry>> {
    let mut out = Vec::new();
    collect_rings(g, &mut out)?;
    Ok(out)
}

fn collect_rings(g: &Geometry, out: &mut Vec<Geometry>) -> Result<()> {
    let kind = g.geometry_type();
    if kind == OGRwkbGeometryType::wkbLinearRing || kind == OGRwkbGeometryType::wkbLineString {
        let mut line = Geometry::empty(OGRwkbGeometryType::wkbLineString)?;
        for (x, y, _) in g.get_point_vec() {
            line.add_point_2d((x, y));
        }
        out.push(line);
        return Ok(());
    }
    for i in 0..g.geometry_count() {
        collect_rings(&g.get_geometry(i), out)?;
    }
    Ok(())
}

/// `buffer` with its alpha plane set: [`coverage`] of `areas`, and 0 for
/// pixels whose every band is the nodata value.
pub fn apply(buffer: PixelBuffer, areas: &[Area], bbox: &Bbox) -> Result<PixelBuffer> {
    let mut alpha = coverage(areas, bbox, buffer.width, buffer.height)?;
    if let Some(nd) = buffer.nodata {
        let one = PixelBuffer::filled(1, 1, 1, buffer.pixel_type, nd, None)?.data;
        let elem = pixel_type_bytes(buffer.pixel_type);
        let px = elem
            .checked_mul(buffer.bands as usize)
            .context("pixel size overflows")?;
        for (pixel, a) in buffer.data.chunks_exact(px).zip(alpha.iter_mut()) {
            if pixel.chunks_exact(elem).all(|s| s == one.as_ref()) {
                *a = 0;
            }
        }
    }
    Ok(PixelBuffer {
        alpha: Some(Bytes::from(alpha)),
        ..buffer
    })
}

/// A `width` x `height` image with no data at all (for tiles and GetMaps
/// outside the source): white, fully transparent.
pub fn empty_image(width: u32, height: u32, bands: u32, pixel_type: PixelType) -> Result<PixelBuffer> {
    let white = if pixel_type == PixelType::U16 { 65535.0 } else { 255.0 };
    let buffer = PixelBuffer::filled(width, height, bands, pixel_type, white, None)?;
    let n = (width as usize)
        .checked_mul(height as usize)
        .context("image size overflows")?;
    Ok(PixelBuffer {
        alpha: Some(Bytes::from(vec![0u8; n])),
        ..buffer
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rd() -> Crs {
        Crs::new("EPSG:28992")
    }

    #[test]
    fn coverage_is_the_intersection_of_areas() {
        let plain = |b| Area {
            trim_edges: false,
            ..rectangle(&Bbox::from(b), &rd()).unwrap()
        };
        let (left, bottom) = (plain([0.0, 0.0, 6.0, 10.0]), plain([0.0, 0.0, 10.0, 4.0]));
        // 10x10 image of [0,0,10,10]: 1 unit per pixel, row 0 at the top.
        let c = coverage(&[left, bottom], &Bbox::from([0.0, 0.0, 10.0, 10.0]), 10, 10).unwrap();
        let covered: Vec<(usize, usize)> = (0..10)
            .flat_map(|r| (0..10).map(move |col| (r, col)))
            .filter(|(r, col)| c[r * 10 + col] == 255)
            .collect();
        assert_eq!(covered.len(), 6 * 4);
        assert!(covered.iter().all(|(r, col)| *col < 6 && *r >= 6));
        assert!(coverage(&[], &Bbox::from([0.0, 0.0, 1.0, 1.0]), 2, 2)
            .unwrap()
            .iter()
            .all(|a| *a == 255));
    }

    /// The outer outline of a dissolved coverage is trimmed by the pixels
    /// it touches; the seam between two neighbouring footprints is not.
    #[test]
    fn dissolved_footprints_trim_only_the_outer_edge() {
        let square = |x0: f64, x1: f64| {
            Geometry::from_wkt(&format!("POLYGON (({x0} 0, {x1} 0, {x1} 10, {x0} 10, {x0} 0))"))
                .unwrap()
                .wkb()
                .unwrap()
        };
        // Two 10x10 tiles side by side, outline offset half a pixel so it
        // cuts through pixels (like GTI's snapped overview edges). Their
        // shared edge misses by float noise, as in the real index.
        let area = dissolve(&[square(0.0, 10.0), square(10.000000051, 20.0)], &rd(), 0.0005).unwrap();
        let c = coverage(&[area], &Bbox::from([-0.5, -0.5, 20.5, 10.5]), 21, 11).unwrap();
        let at = |col: usize, row: usize| c[row * 21 + col];
        assert_eq!(at(0, 5), 0, "outer column is trimmed");
        assert_eq!(at(20, 5), 0, "outer column is trimmed");
        assert_eq!(at(5, 0), 0, "outer row is trimmed");
        assert_eq!(at(10, 5), 255, "the seam between the tiles is not");
        assert_eq!(at(5, 5), 255);
    }

    #[test]
    fn nodata_pixels_get_no_alpha() {
        let buffer = PixelBuffer {
            width: 2,
            height: 1,
            bands: 2,
            pixel_type: PixelType::U8,
            data: Bytes::from(vec![7, 7, 7, 1]),
            nodata: Some(7.0),
            alpha: None,
        };
        let out = apply(buffer, &[], &Bbox::from([0.0, 0.0, 2.0, 1.0])).unwrap();
        assert_eq!(out.alpha.unwrap().as_ref(), &[0, 255]);
    }

    /// The shipped NL mask: everything but Belgium.
    #[test]
    fn shipped_mask_excludes_belgium() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/data/source_mask.shp");
        if !Path::new(path).exists() {
            return;
        }
        let mask = load(path).unwrap();
        assert_eq!(mask.crs, rd());
        let at = |x: f64, y: f64| {
            coverage(
                std::slice::from_ref(&mask),
                &Bbox::from([x - 1.0, y - 1.0, x + 1.0, y + 1.0]),
                1,
                1,
            )
            .unwrap()[0]
        };
        assert_eq!(at(136_846.0, 455_834.0), 255, "Utrecht");
        assert_eq!(at(103_946.0, 293_385.0), 0, "Belgium");
        // Reprojected to Web Mercator, the same point is still masked.
        let wm = reproject(&mask, &Crs::new("EPSG:3857")).unwrap();
        let be = crate::crs::transform_bbox(
            &Bbox::from([103_945.0, 293_384.0, 103_947.0, 293_386.0]),
            &rd(),
            &wm.crs,
        )
        .unwrap();
        assert_eq!(coverage(&[wm], &be, 1, 1).unwrap()[0], 0);
    }
}
