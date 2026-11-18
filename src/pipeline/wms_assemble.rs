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

use anyhow::{anyhow, Context, Result};
use bytes::Bytes;
use gdal::raster::ResampleAlg;
use gdal::vsi::{create_mem_file, unlink_mem_file};
use gdal::Dataset;

use super::error::client_error;
use super::render::{blank_image, layer_resampling, render_grid_tile, Renderer};
use crate::crs::{Bbox, Crs};
use crate::encode::{encode, EncodeOpts, GeoRef, OutputFormat};
use crate::protocol::wms::validate::WmsGetMap;
use crate::source::gdal::raster::{
    bbox_to_geotransform, create_georeferenced_mem, gis_order_spatial_ref, pixel_type_from_gdal, read_pixel_buffer,
    set_nodata, vsimem_path, warp, WarpOptions,
};
use crate::tile::{Pyramid, TileCoord, TileGrid};

/// What the composite produces: owned copies of validated request values,
/// so it can run on the blocking render pool. `crs` is the catalog's config
/// token for the requested CRS — never a client string.
struct Target {
    crs: Crs,
    bbox: Bbox,
    width: u32,
    height: u32,
    format: OutputFormat,
}

/// Serve a validated WMS GetMap end-to-end.
pub async fn serve_get_map(r: &Renderer, q: &WmsGetMap<'_>) -> Result<Bytes> {
    let layer = q.layer().config();
    let grid = q.grid().grid().clone();
    let (width, height) = q.size();
    let bbox = q.bbox();
    let crs = q.crs().crs();

    // BBOX entirely outside the grid: a blank image, not an error.
    if !q.intersects_grid() {
        return blank_image(r, layer, crs, bbox, (width, height), q.format()).await;
    }

    // Pyramid level for the requested resolution, then the covering tiles.
    // Level selection keeps this near (WIDTH/tile_w + 2) x (HEIGHT/tile_h + 2)
    // tiles; the cap is a backstop so no bbox/size combination can explode it.
    let pyramid = Pyramid {
        grid: grid.clone(),
        max_level: grid.level_count().saturating_sub(1),
    };
    let level = pyramid.select_level(&bbox, (width, height));
    let (tw, th) = grid.tile_size();
    let across = |px: u32, tile: u32| u64::from(px).div_ceil(u64::from(tile.max(1))).checked_add(2);
    let max_tiles = across(width, tw)
        .zip(across(height, th))
        .and_then(|(a, b)| a.checked_mul(b))
        .ok_or_else(|| anyhow!("internal: tile budget overflows u64"))?;
    let coords = tile_coords_for_bbox(&grid, level, &bbox, max_tiles)?;

    // Render (or fetch) every tile. Coordinates are server-computed and
    // clamped to the grid.
    let mut tile_bytes: Vec<(TileCoord, Bytes)> = Vec::with_capacity(coords.len());
    for coord in coords {
        let bytes = render_grid_tile(r, layer, &grid, coord, q.format()).await?;
        tile_bytes.push((coord, bytes));
    }

    // Composite into the destination via GDAL warp — CPU-bound and
    // blocking, so it runs on the bounded render pool.
    let resampling = layer_resampling(layer)?;
    let nodata = r.layer_nodata(layer);
    let target = Target { crs: crs.clone(), bbox, width, height, format: q.format() };
    r.cpu
        .run(move || composite_tiles(&grid, &tile_bytes, &target, resampling, nodata))
        .await
}

/// Inclusive `(col, row)` tile rectangle covering `bbox` at `level`.
///
/// Each corner of `bbox` is resolved to a fractional tile index using the
/// grid's level-0 top-left anchor and the per-level resolution, floored, and
/// clamped to the level's tile matrix. Errors (as a client error) when the
/// bbox misses the grid entirely or would need more than `max_tiles` tiles.
fn tile_coords_for_bbox(
    grid: &Arc<dyn TileGrid>,
    level: u32,
    bbox: &Bbox,
    max_tiles: u64,
) -> Result<Vec<TileCoord>> {
    // Recover the grid's top-left anchor from tile (level, 0, 0).
    let tile00 = grid.tile_bbox(TileCoord::new(level, 0, 0));
    let (tlx, tly) = (tile00.minx, tile00.maxy);
    let res = grid.resolution(level);
    let (tw_px, th_px) = grid.tile_size();
    if res.is_nan() || res <= 0.0 || tw_px == 0 || th_px == 0 {
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

    // Clamp to the level's tile matrix. Float→int casts saturate, and the
    // matrix bounds are at most u32::MAX here, so nothing can overflow.
    let (cols, rows) = grid.matrix_size(level);
    let last_col = cols.min(u64::from(u32::MAX) + 1).saturating_sub(1) as f64;
    let last_row = rows.min(u64::from(u32::MAX) + 1).saturating_sub(1) as f64;
    if !(col_lo.is_finite() && col_hi.is_finite() && row_lo.is_finite() && row_hi.is_finite())
        || col_hi < 0.0
        || row_hi < 0.0
        || col_lo > last_col
        || row_lo > last_row
    {
        return Err(client_error("BBOX does not intersect the layer's grid"));
    }
    let col_lo = col_lo.max(0.0) as u32;
    let row_lo = row_lo.max(0.0) as u32;
    // A bbox thinner than the 1e-9 epsilon sitting on a tile edge can floor
    // `hi` below `lo`; it still needs the one tile it touches.
    let col_hi = (col_hi.min(last_col) as u32).max(col_lo);
    let row_hi = (row_hi.min(last_row) as u32).max(row_lo);

    // col_hi >= col_lo and row_hi >= row_lo by construction above.
    let span = |lo: u32, hi: u32| hi.checked_sub(lo).and_then(|d| u64::from(d).checked_add(1));
    let count = span(col_lo, col_hi)
        .zip(span(row_lo, row_hi))
        .and_then(|(c, r)| c.checked_mul(r))
        .ok_or_else(|| anyhow!("internal: tile range is inverted or overflows"))?;
    if count > max_tiles {
        return Err(client_error("BBOX and image size need too many tiles"));
    }

    let mut out = Vec::with_capacity(count as usize);
    for y in row_lo..=row_hi {
        for x in col_lo..=col_hi {
            out.push(TileCoord::new(level, x, y));
        }
    }
    Ok(out)
}

/// GDAL-side composite: open each encoded tile from `/vsimem/`, georeference
/// it to its grid cell, and let a single `GDALWarp` call mosaic and resample
/// all of them into the request's bbox/CRS/size with the layer's kernel and
/// nodata. Then encode to the requested format.
fn composite_tiles(
    grid: &Arc<dyn TileGrid>,
    tiles: &[(TileCoord, Bytes)],
    req: &Target,
    resampling: ResampleAlg,
    nodata: Option<f64>,
) -> Result<Bytes> {
    // Each `/vsimem/` file is unlinked when its guard drops, on every path.
    struct MemFile(String);
    impl Drop for MemFile {
        fn drop(&mut self) {
            let _ = unlink_mem_file(&self.0);
        }
    }

    let tile_sr = gis_order_spatial_ref(grid.crs()).context("composite: tile CRS")?;
    let (tw, th) = grid.tile_size();
    let ext = req.format.extension();
    let mut files = Vec::with_capacity(tiles.len());
    let mut datasets = Vec::with_capacity(tiles.len());
    for (coord, bytes) in tiles {
        let path = vsimem_path("wms-tile", ext);
        create_mem_file(&path, bytes.to_vec())
            .with_context(|| format!("composite: stage tile {coord:?}"))?;
        files.push(MemFile(path.clone()));
        let mut ds = Dataset::open(&path).with_context(|| format!("composite: open tile {coord:?}"))?;
        // Web tile formats carry no georeferencing; place each tile on its cell.
        ds.set_spatial_ref(&tile_sr)
            .with_context(|| format!("composite: set spatial ref on tile {coord:?}"))?;
        ds.set_geo_transform(&bbox_to_geotransform(&grid.tile_bbox(*coord), tw, th)?)
            .with_context(|| format!("composite: set geotransform on tile {coord:?}"))?;
        set_nodata(&ds, nodata)?;
        datasets.push(ds);
    }
    let first = datasets.first().ok_or_else(|| anyhow!("composite_tiles called with no tiles"))?;
    let bands = u32::try_from(first.raster_count()).context("tile band count overflow")?;
    let pixel_type = pixel_type_from_gdal(first.rasterband(1).context("composite: tile band 1")?.band_type())?;

    let mut dst = create_georeferenced_mem(
        pixel_type, req.width, req.height, bands, &req.crs, &req.bbox, nodata,
    )?;
    let sources: Vec<&Dataset> = datasets.iter().collect();
    let opts = WarpOptions { resampling, src_nodata: nodata, dst_nodata: nodata };
    warp(&sources, &mut dst, &opts).context("composite: warping tiles into GetMap extent")?;
    drop(sources);
    drop(datasets);
    drop(files);

    let buffer = read_pixel_buffer(&dst, pixel_type, nodata)?;
    let georef = match req.format {
        OutputFormat::GeoTiff => Some(GeoRef {
            crs: req.crs.clone(),
            bbox: req.bbox,
            nodata,
        }),
        _ => None,
    };
    encode(&buffer, georef.as_ref(), req.format, &EncodeOpts::defaults_for(req.format))
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
    use crate::protocol::catalog::Catalog;
    use crate::protocol::wms::request::parse;
    use crate::protocol::wms::validate::{validate, WmsQuery};
    use crate::cache::LayeredCache;
    use crate::pipeline::cpu::CpuPool;
    use crate::source::gdal::GdalSource;
    use crate::source::{PixelBuffer, PixelType, Source, SourceRegistry};

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

        let r = Renderer::new(
            Arc::new(registry),
            Arc::new(fresh_cache()),
            CpuPool::new(2, Duration::from_secs(5)),
            [(uri.clone(), Arc::new(crate::tile::grid::WebMercator::new()) as Arc<dyn TileGrid>)],
        )
        .unwrap();
        let catalog = world_catalog(&uri);
        // The whole WebMercator world at 256×256.
        let half = 20_037_508.342_789_2_f64;
        let req = world_request(&catalog, 256, 256, [-half, -half, half, half]);

        let bytes = serve_get_map(&r, &req)
            .await
            .expect("serve_get_map");
        assert!(bytes.len() > 8, "PNG too small: {} bytes", bytes.len());
        assert_eq!(
            &bytes[..4],
            &[0x89, 0x50, 0x4E, 0x47],
            "PNG magic missing"
        );

        // Round-trip the response through GDAL via /vsimem/ and confirm size.
        let path = vsimem_path("wms-test", "png");
        create_mem_file(&path, bytes.to_vec()).expect("vsimem write response");
        let ds = Dataset::open(&path).expect("open response from /vsimem/");
        let (w, h) = ds.raster_size();
        drop(ds);
        let _ = unlink_mem_file(&path);
        assert_eq!((w, h), (256, 256));
    }

    fn world_catalog(uri: &str) -> Catalog {
        let config = Config {
            layers: vec![build_layer("world", uri)],
            ..Config::default()
        };
        Catalog::build(&config).expect("catalog")
    }

    /// A GetMap for layer `world` in EPSG:3857, through the real stage 1 +
    /// stage 2 validation (the only way to obtain a `WmsGetMap`).
    fn world_request(catalog: &Catalog, width: u32, height: u32, bbox: [f64; 4]) -> WmsGetMap<'_> {
        let q = format!(
            "SERVICE=WMS&VERSION=1.3.0&REQUEST=GetMap&LAYERS=world&STYLES=&CRS=EPSG:3857\
             &BBOX={},{},{},{}&WIDTH={width}&HEIGHT={height}&FORMAT=image/png",
            bbox[0], bbox[1], bbox[2], bbox[3]
        );
        match validate(parse(&q).expect("stage 1"), catalog).expect("stage 2") {
            WmsQuery::GetMap(m) => m,
            WmsQuery::GetCapabilities { .. } => unreachable!(),
        }
    }

    #[tokio::test]
    async fn bbox_outside_the_grid_returns_a_blank_image() {
        let tmp = write_world_mercator_fixture();
        let uri = tmp.path().to_string_lossy().into_owned();
        let mut registry = SourceRegistry::new();
        registry.insert(
            uri.clone(),
            Arc::new(GdalSource::open(&SourceConfig { uri: uri.clone(), credentials: None }).unwrap()),
        );
        let r = Renderer::new(
            Arc::new(registry),
            Arc::new(fresh_cache()),
            CpuPool::new(1, Duration::from_secs(5)),
            [(uri.clone(), Arc::new(crate::tile::grid::WebMercator::new()) as Arc<dyn TileGrid>)],
        )
        .unwrap();
        let catalog = world_catalog(&uri);
        let req = world_request(&catalog, 64, 32, [1e9, 1e9, 2e9, 2e9]);
        assert!(!req.intersects_grid());
        let bytes = serve_get_map(&r, &req).await.expect("blank image, not an error");
        let path = vsimem_path("wms-test", "png");
        create_mem_file(&path, bytes.to_vec()).unwrap();
        let ds = Dataset::open(&path).unwrap();
        assert_eq!(ds.raster_size(), (64, 32));
        drop(ds);
        let _ = unlink_mem_file(&path);
    }

    #[test]
    fn huge_or_skewed_bboxes_are_clamped_to_a_bounded_tile_set() {
        let grid: Arc<dyn TileGrid> = Arc::new(crate::tile::grid::WebMercator::new());
        let pyramid = Pyramid { grid: grid.clone(), max_level: 21 };
        let half = 20_037_508.342_789_2_f64;
        let cases = [
            // Used to yield ~i64::MAX tile coordinates and abort the process.
            ([-1e300, -1e300, 1e300, 1e300], (1, 1)),
            // Tiny width, full-world height: level must follow the coarser axis.
            ([0.0, -half, 1e-3, half], (1, 1)),
            ([0.0, -half, 1e-3, half], (2500, 2500)),
        ];
        for (bbox, size) in cases {
            let bbox = Bbox::from(bbox);
            let level = pyramid.select_level(&bbox, size);
            let coords = tile_coords_for_bbox(&grid, level, &bbox, 121).unwrap();
            assert!(
                coords.len() <= 121,
                "{bbox:?} @ {size:?}: {} tiles at level {level}",
                coords.len()
            );
        }
        // The backstop cap itself is enforced.
        let bbox = Bbox::from([-half, -half, half, half]);
        assert!(tile_coords_for_bbox(&grid, 10, &bbox, 121).is_err());
    }

    /// Property: whatever BBOX/size/version a client sends, GetMap ends in
    /// an image of the requested size or a client error (400) — never an
    /// internal error (500). Coordinates mix arbitrary floats (incl. NaN,
    /// ±inf, subnormals, ±f64::MAX) with values a hair off grid and tile
    /// edges, where precision trouble lives.
    #[test]
    fn hostile_bboxes_never_cause_internal_errors() {
        use crate::pipeline::error::{classify, Disposition};
        use proptest::prelude::*;
        use proptest::test_runner::{Config as PtConfig, TestRunner};

        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let tmp = write_world_mercator_fixture();
        let uri = tmp.path().to_string_lossy().into_owned();
        let mut registry = SourceRegistry::new();
        registry.insert(uri.clone(), Arc::new(GdalSource::open(&SourceConfig { uri: uri.clone(), credentials: None }).unwrap()));
        let r = Renderer::new(
            Arc::new(registry),
            Arc::new(fresh_cache()),
            CpuPool::new(2, Duration::from_secs(30)),
            [(uri.clone(), Arc::new(crate::tile::grid::WebMercator::new()) as Arc<dyn TileGrid>)],
        )
        .unwrap();
        let mut layer = build_layer("world", &uri);
        layer.formats.push(OutputFormat::GeoTiff);
        let catalog = Catalog::build(&Config { layers: vec![layer], ..Config::default() }).unwrap();

        let half = 20_037_508.342_789_2_f64;
        let edge = prop_oneof![
            Just(0.0),
            Just(half),
            Just(-half),
            (0u32..20).prop_map(move |z| half / f64::from(1u32 << z)),
            Just(1e9),
            Just(1e300),
            Just(-1e300),
            Just(f64::MAX),
            Just(f64::MIN),
        ];
        let delta = prop_oneof![
            Just(0.0),
            Just(5e-324),
            Just(-5e-324),
            Just(f64::MIN_POSITIVE),
            Just(1e-300),
            Just(1e-9),
            Just(-1e-9),
            Just(1.0),
            -1e6..1e6f64,
        ];
        let coord = prop_oneof![
            3 => (edge, delta).prop_map(|(e, d)| e + d),
            1 => any::<f64>(),
            1 => -3e7..3e7f64,
        ];
        let strategy = (
            [coord.clone(), coord.clone(), coord.clone(), coord],
            1u32..=300,
            1u32..=300,
            any::<bool>(),
            any::<bool>(),
        );
        // How many cases got past validation into the renderer.
        let rendered = std::sync::atomic::AtomicU32::new(0);
        let mut runner = TestRunner::new(PtConfig { cases: 128, failure_persistence: None, ..PtConfig::default() });
        runner
            .run(&strategy, |([a, b, c, d], w, h, tiff, v111)| {
                // Mostly well-ordered boxes: an inverted one only tests stage 1.
                let bbox = [a.min(c), b.min(d), a.max(c), b.max(d)];
                let (version, key) = if v111 { ("1.1.1", "SRS") } else { ("1.3.0", "CRS") };
                let format = if tiff { "image/tiff" } else { "image/png" };
                let q = format!(
                    "SERVICE=WMS&VERSION={version}&REQUEST=GetMap&LAYERS=world&STYLES=&{key}=EPSG:3857\
                     &BBOX={:e},{:e},{:e},{:e}&WIDTH={w}&HEIGHT={h}&FORMAT={format}",
                    bbox[0], bbox[1], bbox[2], bbox[3]
                );
                let failure: Option<String> = rt.block_on(async {
                    let m = match parse(&q).and_then(|p| validate(p, &catalog)) {
                        Ok(WmsQuery::GetMap(m)) => m,
                        Ok(WmsQuery::GetCapabilities { .. }) => return Some("parsed as GetCapabilities".into()),
                        Err(_) => return None, // an OGC exception: 400
                    };
                    rendered.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    match serve_get_map(&r, &m).await {
                        Ok(bytes) => {
                            let path = vsimem_path("wms-prop", "bin");
                            create_mem_file(&path, bytes.to_vec()).unwrap();
                            let ds = Dataset::open(&path).unwrap();
                            let size = ds.raster_size();
                            let gt = ds.geo_transform().ok();
                            drop(ds);
                            let _ = unlink_mem_file(&path);
                            if size != (w as usize, h as usize) {
                                return Some(format!("image is {size:?}"));
                            }
                            match gt {
                                Some(gt) if tiff && !gt.iter().all(|v| v.is_finite()) => {
                                    Some(format!("non-finite geotransform {gt:?}"))
                                }
                                _ => None,
                            }
                        }
                        Err(e) => match classify(&e) {
                            Disposition::Client(_) => None,
                            _ => Some(format!("internal error: {e:#}")),
                        },
                    }
                });
                prop_assert!(failure.is_none(), "{q}: {}", failure.unwrap_or_default());
                Ok(())
            })
            .unwrap();
        // Guard against a generator that only exercises validation.
        let rendered = rendered.into_inner();
        println!("hostile bboxes: {rendered}/128 reached the renderer");
        assert!(rendered >= 32, "only {rendered}/128 cases reached the renderer");
    }
}
