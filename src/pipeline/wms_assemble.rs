//! `serve_get_map` — assemble a WMS GetMap response from tile-cache reads.
//!
//! See `docs/src/scope-and-protocols.md` (Architecture): WMS GetMap is
//! decomposed into the tile grid that covers the request, each tile is fetched
//! (or rendered) through the same cache pipeline as WMTS/TMS, and the tiles
//! are reprojected and composited into the response image.
//!
//! Strategy:
//!
//! 1. Resolve the layer + a grid whose CRS matches the request CRS.
//! 2. Pick a pyramid level matching the request resolution.
//! 3. Compute the rectangle of `(col, row)` tile coordinates that covers
//!    `req.bbox` at that level.
//! 4. For each tile, call [`render_tile`] (cache-aware).
//! 5. Decode each tile via GDAL (`/vsimem/` round-trip), reproject into a
//!    destination MEM dataset georeferenced to `req.bbox` in `req.crs`,
//!    then read the destination back as a `PixelBuffer` and encode.
//!
//! GDAL's `reproject` handles edge cases for free: tiles that only partially
//! intersect `req.bbox`, resampling between the tile grid's resolution and
//! the requested output resolution, and any CRS-to-CRS warping needed (in
//! practice the tile grid and request share a CRS in v1, but the same code
//! path works either way).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use gdal::raster::{Buffer, GdalDataType, GdalType};
use gdal::spatial_ref::SpatialRef;
use gdal::vsi::{create_mem_file, unlink_mem_file};
use gdal::{Dataset, DriverManager};

use super::render::render_tile;
use crate::cache::LayeredCache;
use crate::config::{Config, LayerConfig};
use crate::crs::{Bbox, Crs};
use crate::encode::{encode, EncodeOpts, GeoRef, OutputFormat};
use crate::protocol::wms::request::GetMapRequest;
use crate::source::{PixelBuffer, PixelType, SourceRegistry};
use crate::tile::grid::lookup_grid;
use crate::tile::{Pyramid, TileCoord, TileGrid};

/// Monotonic counter for minting unique `/vsimem/` paths per request.
static VSIMEM_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Handle a parsed WMS GetMap request end-to-end.
pub async fn serve_get_map(
    sources: &SourceRegistry,
    cache: &LayeredCache,
    config: &Config,
    req: &GetMapRequest,
) -> Result<Bytes> {
    // 1. Resolve the layer. Multi-layer GetMap is out of scope for v1; we
    //    pick the first listed layer (which must exist).
    let layer_name = req
        .layers
        .first()
        .ok_or_else(|| anyhow!("GetMap request carried no LAYERS"))?;
    let layer = config
        .layers
        .iter()
        .find(|l| &l.name == layer_name)
        .ok_or_else(|| anyhow!("layer `{layer_name}` not found in config"))?;

    // 2. Pick the first of the layer's grids whose CRS matches the request.
    let grid = pick_grid_for_crs(layer, config, &req.crs)?;

    // 3. Build a pyramid and select the pyramid level.
    let pyramid = Pyramid {
        grid: grid.clone(),
        max_level: grid.level_count().saturating_sub(1),
    };
    let level = pyramid.select_level(&req.bbox, (req.width, req.height));

    // 4. Compute the covering tile rectangle at `level`.
    let coords = tile_coords_for_bbox(&grid, level, &req.bbox)?;
    if coords.is_empty() {
        return Err(anyhow!(
            "no tiles cover the requested bbox at level {level} for grid `{}`",
            grid.name()
        ));
    }

    // 5. Render (or fetch) every tile.
    let mut tile_bytes: Vec<(TileCoord, Bytes)> = Vec::with_capacity(coords.len());
    for coord in coords {
        let bytes = render_tile(sources, cache, layer, &grid, coord, req.format).await?;
        tile_bytes.push((coord, bytes));
    }

    // 6. Composite into the destination via GDAL `reproject`.
    composite_tiles(&grid, &tile_bytes, req)
}

/// Find a grid whose CRS matches `crs`, walking `layer.grids` in declared
/// order. Errors if no grid matches.
fn pick_grid_for_crs(
    layer: &LayerConfig,
    config: &Config,
    crs: &Crs,
) -> Result<Arc<dyn TileGrid>> {
    for name in &layer.grids {
        let grid = lookup_grid(name, &config.grids).ok_or_else(|| {
            anyhow!(
                "layer `{}` references unknown grid `{}`",
                layer.name,
                name
            )
        })?;
        if grid.crs() == crs {
            return Ok(grid);
        }
    }
    Err(anyhow!(
        "no grid declared by layer `{}` matches CRS `{}`",
        layer.name,
        crs.as_str()
    ))
}

/// Inclusive `(col, row)` tile rectangle covering `bbox` at `level`.
///
/// We resolve each corner of `bbox` to a fractional tile index using the
/// grid's level-0 top-left anchor and the per-level resolution, then floor
/// to integer tiles and clamp to the level's tile grid extent.
fn tile_coords_for_bbox(
    grid: &Arc<dyn TileGrid>,
    level: u32,
    bbox: &Bbox,
) -> Result<Vec<TileCoord>> {
    // Recover the grid's top-left anchor from tile (level, 0, 0).
    let tile00 = grid.tile_bbox(TileCoord::new(level, 0, 0));
    let (tlx, tly) = (tile00.minx, tile00.maxy);
    let res = grid.resolution(level);
    let (tw_px, th_px) = grid.tile_size();
    if res <= 0.0 || tw_px == 0 || th_px == 0 {
        return Err(anyhow!(
            "grid `{}` has degenerate level-{level} parameters: res={res} tile_size=({tw_px},{th_px})",
            grid.name()
        ));
    }
    let tile_w_units = tw_px as f64 * res;
    let tile_h_units = th_px as f64 * res;

    // Forward map: (minx → leftmost col), (maxy → topmost row),
    //              (maxx → rightmost col), (miny → bottommost row).
    let col_lo = ((bbox.minx - tlx) / tile_w_units).floor();
    let col_hi = ((bbox.maxx - tlx) / tile_w_units - 1e-9).floor();
    let row_lo = ((tly - bbox.maxy) / tile_h_units).floor();
    let row_hi = ((tly - bbox.miny) / tile_h_units - 1e-9).floor();

    // Clamp to the level's valid tile range. A `QuadGrid`-style level has
    // `2^level` tiles per axis on standard powers-of-two grids; we can't
    // know that for arbitrary `TileGrid`s without an extra trait method, so
    // we accept negative indices being clamped to 0 and trust the upper end
    // (GDAL `reproject` will handle out-of-extent tiles as no-ops at edges
    // if we pre-clamp the lower bound to 0).
    let col_lo = col_lo.max(0.0) as i64;
    let col_hi = col_hi.max(0.0) as i64;
    let row_lo = row_lo.max(0.0) as i64;
    let row_hi = row_hi.max(0.0) as i64;

    let mut out = Vec::new();
    for y in row_lo..=row_hi {
        for x in col_lo..=col_hi {
            out.push(TileCoord::new(level, x as u32, y as u32));
        }
    }
    Ok(out)
}

/// GDAL-side composite: decode each tile, reproject into a single destination
/// MEM dataset, read back as a `PixelBuffer`, and encode to the request format.
fn composite_tiles(
    grid: &Arc<dyn TileGrid>,
    tiles: &[(TileCoord, Bytes)],
    req: &GetMapRequest,
) -> Result<Bytes> {
    // Probe the first tile to discover band count + pixel type. We assume the
    // tile cache returns homogeneous bands/dtype within one GetMap response,
    // which is the contract `render_tile` provides for one (layer, format).
    let (first_coord, first_bytes) = tiles
        .first()
        .ok_or_else(|| anyhow!("composite_tiles called with no tiles"))?;
    let first_vsi = mint_vsimem_path(req.format.extension(), 0);
    create_mem_file(&first_vsi, first_bytes.to_vec())
        .with_context(|| format!("composite: write probe tile to {first_vsi}"))?;
    let probe = Dataset::open(&first_vsi)
        .with_context(|| format!("composite: open probe tile at {first_vsi}"))?;
    let bands = probe.raster_count();
    if bands == 0 {
        let _ = unlink_mem_file(&first_vsi);
        return Err(anyhow!("composite: tile dataset has zero bands"));
    }
    let band1 = probe
        .rasterband(1)
        .context("composite: failed to fetch probe band 1")?;
    let pixel_type = pixel_type_from_gdal(band1.band_type())?;
    let nodata = band1.no_data_value();
    drop(band1);
    drop(probe);
    let _ = unlink_mem_file(&first_vsi);
    let _ = first_coord; // already consumed via tiles[0]; named for clarity above.

    // Build the destination MEM dataset, sized + georeferenced to the request.
    let mem_driver = DriverManager::get_driver_by_name("MEM")
        .context("composite: MEM driver not available")?;
    let mut dst = create_mem_for_type(&mem_driver, pixel_type, req.width, req.height, bands as u32)?;
    let dst_sr = SpatialRef::from_definition(req.crs.as_str())
        .with_context(|| format!("composite: invalid request CRS '{}'", req.crs.as_str()))?;
    dst.set_spatial_ref(&dst_sr)
        .context("composite: failed to set destination spatial ref")?;
    dst.set_geo_transform(&bbox_to_geotransform(&req.bbox, req.width, req.height))
        .context("composite: failed to set destination geotransform")?;
    // Carry nodata through so reproject treats unwritten edge pixels predictably.
    if let Some(nd) = nodata {
        for b in 1..=bands {
            let mut band = dst
                .rasterband(b)
                .with_context(|| format!("composite: failed to fetch dst band {b}"))?;
            band.set_no_data_value(Some(nd))
                .with_context(|| format!("composite: failed to set nodata on dst band {b}"))?;
        }
    }

    // Reproject each tile into `dst`. Each tile is opened from a fresh
    // `/vsimem/` path; the path's bytes come from the cache. We set the
    // tile's georeferencing to the grid's `tile_bbox(coord)` in the grid's
    // CRS so GDAL can do the placement and any resampling/reprojection.
    let tile_crs = grid.crs().clone();
    let tile_sr = SpatialRef::from_definition(tile_crs.as_str())
        .with_context(|| format!("composite: invalid tile CRS '{}'", tile_crs.as_str()))?;
    let (tw_px, th_px) = grid.tile_size();
    let ext = req.format.extension();
    for (idx, (coord, bytes)) in tiles.iter().enumerate() {
        // `idx + 1` to keep the probe-tile slot reserved for diagnostics.
        let vsi_path = mint_vsimem_path(ext, idx as u64 + 1);
        create_mem_file(&vsi_path, bytes.to_vec())
            .with_context(|| format!("composite: write tile bytes to {vsi_path}"))?;

        // Some web-tile formats (PNG / JPEG / WebP) are not georeferenced on
        // the wire — set the SRS + geotransform on the opened dataset so
        // `reproject` can place it correctly.
        let mut tile_ds = match Dataset::open(&vsi_path) {
            Ok(d) => d,
            Err(e) => {
                let _ = unlink_mem_file(&vsi_path);
                return Err(anyhow!(e))
                    .with_context(|| format!("composite: open tile {coord:?} from {vsi_path}"));
            }
        };
        if let Err(e) = tile_ds.set_spatial_ref(&tile_sr) {
            let _ = unlink_mem_file(&vsi_path);
            return Err(anyhow!(e)).with_context(|| format!(
                "composite: set spatial ref on tile {coord:?}"
            ));
        }
        let tile_bbox = grid.tile_bbox(*coord);
        let gt = bbox_to_geotransform(&tile_bbox, tw_px, th_px);
        if let Err(e) = tile_ds.set_geo_transform(&gt) {
            let _ = unlink_mem_file(&vsi_path);
            return Err(anyhow!(e)).with_context(|| format!(
                "composite: set geotransform on tile {coord:?}"
            ));
        }

        let reproject_result = gdal::raster::reproject(&tile_ds, &dst);
        // Always unlink, regardless of reproject success/failure.
        drop(tile_ds);
        let _ = unlink_mem_file(&vsi_path);
        reproject_result.with_context(|| format!(
            "composite: reproject failed for tile {coord:?}"
        ))?;
    }

    // Read the destination back into a PixelBuffer and encode.
    let buffer = read_dataset_as_pixelbuffer(&dst, pixel_type, bands as u32, req.width, req.height, nodata)?;
    let georef = match req.format {
        OutputFormat::GeoTiff => Some(GeoRef {
            crs: req.crs.clone(),
            bbox: req.bbox,
            nodata,
        }),
        _ => None,
    };
    encode(
        &buffer,
        georef.as_ref(),
        req.format,
        &EncodeOpts::defaults_for(req.format),
    )
}

/// Pull all bands out of `ds` and interleave into a `PixelBuffer`.
fn read_dataset_as_pixelbuffer(
    ds: &Dataset,
    pixel_type: PixelType,
    bands: u32,
    width: u32,
    height: u32,
    nodata: Option<f64>,
) -> Result<PixelBuffer> {
    let mut planes: Vec<Vec<u8>> = Vec::with_capacity(bands as usize);
    for band_idx in 1..=bands as usize {
        let band = ds
            .rasterband(band_idx)
            .with_context(|| format!("composite: read dst band {band_idx}"))?;
        let plane = match pixel_type {
            PixelType::U8 => read_full_band::<u8>(&band)?,
            PixelType::U16 => read_full_band::<u16>(&band)?,
            PixelType::I16 => read_full_band::<i16>(&band)?,
            PixelType::U32 => read_full_band::<u32>(&band)?,
            PixelType::I32 => read_full_band::<i32>(&band)?,
            PixelType::F32 => read_full_band::<f32>(&band)?,
            PixelType::F64 => read_full_band::<f64>(&band)?,
        };
        planes.push(plane);
    }
    let data = interleave_bands(&planes, width, height, bands, pixel_type)?;
    Ok(PixelBuffer {
        width,
        height,
        bands,
        pixel_type,
        data: Bytes::from(data),
        nodata,
    })
}

fn read_full_band<T: GdalType + Copy>(band: &gdal::raster::RasterBand) -> Result<Vec<u8>> {
    let buf: Buffer<T> = band
        .read_band_as::<T>()
        .context("composite: read_band_as on destination")?;
    let (_, vec) = buf.into_shape_and_vec();
    Ok(typed_vec_to_ne_bytes(vec))
}

fn typed_vec_to_ne_bytes<T: Copy>(vec: Vec<T>) -> Vec<u8> {
    let mut out = Vec::with_capacity(vec.len() * std::mem::size_of::<T>());
    for v in vec {
        // SAFETY: `T: Copy` and we only read the bytes of a local for the
        // duration of this loop iteration. Mirrors the helper in `source/gdal/warp.rs`.
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(
                (&v as *const T) as *const u8,
                std::mem::size_of::<T>(),
            )
        };
        out.extend_from_slice(bytes);
    }
    out
}

fn interleave_bands(
    planes: &[Vec<u8>],
    width: u32,
    height: u32,
    bands: u32,
    pixel_type: PixelType,
) -> Result<Vec<u8>> {
    let elem = pixel_type_bytes(pixel_type);
    let pixels = (width as usize) * (height as usize);
    let plane_bytes = pixels * elem;
    for (i, p) in planes.iter().enumerate() {
        if p.len() != plane_bytes {
            return Err(anyhow!(
                "composite band {} byte length {} != expected {} (w={} h={} elem={})",
                i + 1,
                p.len(),
                plane_bytes,
                width,
                height,
                elem
            ));
        }
    }
    let mut out = vec![0u8; pixels * (bands as usize) * elem];
    for pix in 0..pixels {
        for band_idx in 0..bands as usize {
            let src = &planes[band_idx][pix * elem..(pix + 1) * elem];
            let dst_off = (pix * bands as usize + band_idx) * elem;
            out[dst_off..dst_off + elem].copy_from_slice(src);
        }
    }
    Ok(out)
}

fn pixel_type_bytes(pt: PixelType) -> usize {
    match pt {
        PixelType::U8 => 1,
        PixelType::U16 | PixelType::I16 => 2,
        PixelType::U32 | PixelType::I32 | PixelType::F32 => 4,
        PixelType::F64 => 8,
    }
}

fn pixel_type_from_gdal(dt: GdalDataType) -> Result<PixelType> {
    Ok(match dt {
        GdalDataType::UInt8 => PixelType::U8,
        GdalDataType::UInt16 => PixelType::U16,
        GdalDataType::Int16 => PixelType::I16,
        GdalDataType::UInt32 => PixelType::U32,
        GdalDataType::Int32 => PixelType::I32,
        GdalDataType::Float32 => PixelType::F32,
        GdalDataType::Float64 => PixelType::F64,
        other => return Err(anyhow!("composite: unsupported GDAL data type: {other}")),
    })
}

fn create_mem_for_type(
    driver: &gdal::Driver,
    pt: PixelType,
    width: u32,
    height: u32,
    bands: u32,
) -> Result<Dataset> {
    let w = width as usize;
    let h = height as usize;
    let b = bands as usize;
    match pt {
        PixelType::U8 => driver.create_with_band_type::<u8, _>("kwms-wms-dst", w, h, b),
        PixelType::U16 => driver.create_with_band_type::<u16, _>("kwms-wms-dst", w, h, b),
        PixelType::I16 => driver.create_with_band_type::<i16, _>("kwms-wms-dst", w, h, b),
        PixelType::U32 => driver.create_with_band_type::<u32, _>("kwms-wms-dst", w, h, b),
        PixelType::I32 => driver.create_with_band_type::<i32, _>("kwms-wms-dst", w, h, b),
        PixelType::F32 => driver.create_with_band_type::<f32, _>("kwms-wms-dst", w, h, b),
        PixelType::F64 => driver.create_with_band_type::<f64, _>("kwms-wms-dst", w, h, b),
    }
    .context("composite: failed to create MEM destination dataset")
}

/// `[origin_x, pixel_width, 0, origin_y, 0, -pixel_height]`.
fn bbox_to_geotransform(bbox: &Bbox, width: u32, height: u32) -> [f64; 6] {
    let pixel_width = (bbox.maxx - bbox.minx) / (width as f64);
    let pixel_height = (bbox.maxy - bbox.miny) / (height as f64);
    [bbox.minx, pixel_width, 0.0, bbox.maxy, 0.0, -pixel_height]
}

fn mint_vsimem_path(ext: &str, salt: u64) -> String {
    let id = VSIMEM_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("/vsimem/kwms-wms-{id}-{salt}.{ext}")
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;
    use std::sync::Arc;
    use std::time::Duration;

    use bytes::Bytes;
    use opendal::{services, Operator};
    use tempfile::NamedTempFile;

    use crate::cache::hot::HotTier;
    use crate::cache::persistent::PersistentTier;
    use crate::config::model::{
        CacheMode, Config, LayerCacheConfig, LayerConfig, MaxResolution, MaxResolutionMode,
        SourceConfig,
    };
    use crate::crs::{Bbox, Crs};
    use crate::encode::{encode, EncodeOpts, GeoRef, OutputFormat};
    use crate::protocol::wms::request::WmsVersion;
    use crate::source::gdal::GdalSource;
    use crate::source::{PixelBuffer, PixelType, Source};

    /// EPSG:3857 RGB GeoTIFF fixture covering the WebMercator world extent.
    /// Using the grid's native CRS lets the tile renderer skip the reprojection
    /// step, which keeps the fixture small and the test self-contained.
    fn write_world_mercator_fixture() -> NamedTempFile {
        let width: u32 = 128;
        let height: u32 = 128;
        let bands: u32 = 3;
        let mut data: Vec<u8> = Vec::with_capacity((width * height * bands) as usize);
        for row in 0..height {
            for col in 0..width {
                data.push((col as u8).wrapping_mul(2));
                data.push((row as u8).wrapping_mul(2));
                data.push(((row + col) as u8).wrapping_mul(2));
            }
        }
        let buffer = PixelBuffer {
            width,
            height,
            bands,
            pixel_type: PixelType::U8,
            data: Bytes::from(data),
            nodata: None,
        };
        let half = 20_037_508.342_789_2_f64;
        let georef = GeoRef {
            crs: Crs::new("EPSG:3857"),
            bbox: Bbox::from([-half, -half, half, half]),
            nodata: None,
        };
        let bytes = encode(
            &buffer,
            Some(&georef),
            OutputFormat::GeoTiff,
            &EncodeOpts::defaults_for(OutputFormat::GeoTiff),
        )
        .expect("encode mercator world fixture");

        let mut tmp = tempfile::Builder::new()
            .prefix("kwms-pipeline-wms-")
            .suffix(".tif")
            .tempfile()
            .expect("tempfile");
        tmp.write_all(&bytes).expect("write fixture");
        tmp.flush().expect("flush fixture");
        tmp
    }

    fn fresh_cache() -> LayeredCache {
        let op = Operator::new(services::Memory::default())
            .expect("memory builder")
            .finish();
        LayeredCache::from_parts(HotTier::new(16 * 1024 * 1024), PersistentTier::new(op))
    }

    fn build_layer(name: &str, uri: &str) -> LayerConfig {
        LayerConfig {
            name: name.into(),
            grids: vec!["webmercator".into()],
            max_resolution: MaxResolution::Mode(MaxResolutionMode::Native),
            bbox: None,
            sources: vec![SourceConfig {
                uri: uri.into(),
                credentials: None,
            }],
            cache: LayerCacheConfig {
                mode: CacheMode::Cache,
                ttl: Duration::from_secs(60),
                level_rules: Vec::new(),
            },
            title: name.into(),
            abstract_: String::new(),
            keywords: Vec::new(),
            attribution: None,
            metadata_url: None,
            formats: vec![OutputFormat::Png],
            resampling: "bilinear".into(),
            queryable: true,
            opaque: false,
            nodata: None,
        }
    }

    #[tokio::test]
    async fn serve_get_map_produces_256x256_png() {
        let tmp = write_world_mercator_fixture();
        let uri = tmp.path().to_string_lossy().into_owned();

        let mut registry = SourceRegistry::new();
        let source: Arc<dyn Source> = Arc::new(
            GdalSource::open(&SourceConfig {
                uri: uri.clone(),
                credentials: None,
            })
            .expect("open fixture"),
        );
        registry.insert(uri.clone(), source);

        let cache = fresh_cache();
        let layer = build_layer("world", &uri);
        let config = Config {
            layers: vec![layer.clone()],
            ..Config::default()
        };

        // The whole WebMercator world at 256×256.
        let half = 20_037_508.342_789_2_f64;
        let req = GetMapRequest {
            version: WmsVersion::V1_3_0,
            layers: vec!["world".into()],
            styles: vec![],
            crs: Crs::new("EPSG:3857"),
            bbox: Bbox::from([-half, -half, half, half]),
            width: 256,
            height: 256,
            format: OutputFormat::Png,
            transparent: false,
            bgcolor: None,
        };

        let bytes = serve_get_map(&registry, &cache, &config, &req)
            .await
            .expect("serve_get_map");
        assert!(bytes.len() > 8, "PNG too small: {} bytes", bytes.len());
        assert_eq!(
            &bytes[..4],
            &[0x89, 0x50, 0x4E, 0x47],
            "PNG magic missing"
        );

        // Round-trip the response through GDAL via /vsimem/ and confirm size.
        let id = super::VSIMEM_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = format!("/vsimem/kwms-wms-test-{id}.png");
        create_mem_file(&path, bytes.to_vec()).expect("vsimem write response");
        let ds = Dataset::open(&path).expect("open response from /vsimem/");
        let (w, h) = ds.raster_size();
        drop(ds);
        let _ = unlink_mem_file(&path);
        assert_eq!((w, h), (256, 256));
    }
}
