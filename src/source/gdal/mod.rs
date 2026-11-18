//! GDAL-backed [`Source`] implementation.
//!
//! One of three modules permitted to call into the `gdal` crate directly
//! (alongside `crs/gdal.rs` and `encode.rs`).

pub mod dataset;
pub mod pool;
pub mod sample;
pub(crate) mod raster;
pub mod warp;

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};

use crate::config::SourceConfig;
use gdal::GeoTransformEx;

use crate::crs::{identify_crs, Bbox, Coord, Crs};

use self::pool::DatasetPool;
use self::raster::pixel_type_from_gdal;
use self::warp::read_window as warp_read_window;

use super::{PixelBuffer, PixelValue, Source, SourceMetadata, WindowRequest};

/// Default per-source pool size if not overridden. Kept small because GDAL
/// dataset handles (especially VRT mosaics and VSI-backed handles) are
/// memory-heavy and we'd rather queue than balloon RAM.
const DEFAULT_POOL_SIZE: usize = 4;

pub struct GdalSource {
    pool: DatasetPool,
    metadata: SourceMetadata,
}

impl GdalSource {
    /// Open the source described by `config`. The constructor opens the
    /// pool's first dataset and probes it for metadata; this catches
    /// bad URIs, missing credentials, and unsupported pixel types at
    /// startup rather than on first request.
    pub fn open(config: &SourceConfig) -> Result<Self> {
        // For phase 2 the `credentials` field is just an opaque named
        // reference into `secrets.yaml`; full secrets wiring is the job of
        // a later phase. We accept an empty credential map here and let
        // the future plumbing layer pass real key/value blocks in.
        let credentials: HashMap<String, String> = HashMap::new();
        Self::open_with_credentials(&config.uri, credentials, DEFAULT_POOL_SIZE)
    }

    /// Test-friendly constructor that takes explicit credentials and pool
    /// size. The public `open` is the production entry point.
    pub fn open_with_credentials(
        uri: &str,
        credentials: HashMap<String, String>,
        pool_size: usize,
    ) -> Result<Self> {
        let pool = DatasetPool::new(uri, credentials, pool_size)
            .with_context(|| format!("failed to build dataset pool for '{uri}'"))?;
        let metadata = futures_executor_block_on(probe_metadata(&pool))
            .with_context(|| format!("failed to probe metadata for '{uri}'"))?;
        Ok(Self { pool, metadata })
    }
}

/// Tiny synchronous adaptor — we only block once at startup, not on the
/// hot path. We avoid pulling in `futures` for one call; use Tokio's
/// runtime if present, otherwise build a minimal current-thread one.
fn futures_executor_block_on<F>(fut: F) -> F::Output
where
    F: std::future::Future + Send,
    F::Output: Send,
{
    // If we're already in a Tokio runtime context (e.g. server startup or
    // tests with `#[tokio::test]`), spawn a temporary single-thread runtime
    // in a worker thread to avoid the "cannot block within an async context"
    // panic. Otherwise build one directly.
    match tokio::runtime::Handle::try_current() {
        Ok(_) => std::thread::scope(|s| {
            s.spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("temporary tokio runtime for blocking metadata probe");
                rt.block_on(fut)
            })
            .join()
            .expect("blocking metadata probe thread panicked")
        }),
        Err(_) => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio runtime for blocking metadata probe");
            rt.block_on(fut)
        }
    }
}

/// One-shot metadata probe: run through the pool to inspect the dataset.
async fn probe_metadata(pool: &DatasetPool) -> Result<SourceMetadata> {
    pool.with_dataset(|ds| -> Result<SourceMetadata> {
        let bands = ds.raster_count();
        if bands == 0 {
            return Err(anyhow!("dataset has zero raster bands"));
        }
        let band1 = ds.rasterband(1).context("failed to get band 1")?;
        let pixel_type = pixel_type_from_gdal(band1.band_type())?;
        let nodata = band1.no_data_value();
        let count = usize::try_from(band1.overview_count().context("counting overviews")?)
            .context("negative overview count")?;
        let to_u32 = |(w, h): (usize, usize)| -> Result<(u32, u32)> {
            Ok((u32::try_from(w).context("raster too wide")?, u32::try_from(h).context("raster too tall")?))
        };
        let overviews = (0..count)
            .map(|i| to_u32(band1.overview(i).context("reading an overview")?.size()))
            .collect::<Result<Vec<_>>>()?;
        drop(band1);

        // Native CRS: let GDAL identify it (e.g. an ESRI-WKT RD New becomes
        // `EPSG:28992`), so it compares equal to grid CRSes; unidentifiable
        // definitions stay as WKT.
        let wkt = ds.projection();
        if wkt.is_empty() {
            return Err(anyhow!("dataset has no spatial reference / projection"));
        }
        let native_crs = identify_crs(&Crs::new(wkt)).context("identifying the dataset's CRS")?;

        // Extent from all four corners through the geotransform, so rotated
        // or sheared geotransforms are covered too.
        let gt = ds.geo_transform().context("dataset has no geotransform")?;
        let (w, h) = ds.raster_size();
        let (w, h) = (w as f64, h as f64);
        let corners = [gt.apply(0.0, 0.0), gt.apply(w, 0.0), gt.apply(0.0, h), gt.apply(w, h)];
        let fold = |f: fn(f64, f64) -> f64, init: f64, pick: fn(&(f64, f64)) -> f64| {
            corners.iter().map(pick).fold(init, f)
        };
        let native_bbox = Bbox {
            minx: fold(f64::min, f64::INFINITY, |c| c.0),
            maxx: fold(f64::max, f64::NEG_INFINITY, |c| c.0),
            miny: fold(f64::min, f64::INFINITY, |c| c.1),
            maxy: fold(f64::max, f64::NEG_INFINITY, |c| c.1),
        };
        // Ground size of one pixel along the raster's x axis.
        let native_resolution = gt[1].hypot(gt[4]);
        let size = to_u32(ds.raster_size())?;

        Ok(SourceMetadata {
            native_crs,
            native_bbox,
            band_count: bands as u32,
            pixel_type,
            native_resolution,
            nodata,
            size,
            overviews,
        })
    })
    .await
}

#[async_trait::async_trait]
impl Source for GdalSource {
    fn metadata(&self) -> &SourceMetadata {
        &self.metadata
    }

    async fn read_window(&self, req: WindowRequest) -> Result<PixelBuffer> {
        self.pool
            .with_dataset(move |ds| warp_read_window(ds, &req))
            .await
    }

    async fn sample(&self, coord: Coord, crs: Crs) -> Result<Vec<PixelValue>> {
        let native_crs = self.metadata.native_crs.clone();
        self.pool
            .with_dataset(move |ds| sample::sample(ds, &native_crs, coord, crs))
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::io::Write;

    use bytes::Bytes;
    use tempfile::NamedTempFile;

    use crate::crs::{Bbox, Coord, Crs};
    use crate::encode::{encode, EncodeOpts, GeoRef, OutputFormat};
    use crate::source::{PixelBuffer, PixelType, Source, WindowRequest};
    use gdal::raster::ResampleAlg;

    /// Build a small RGB u8 GeoTIFF fixture at a temp path and return the
    /// path. Bbox is EPSG:4326 [0, 0, 1, 1]; size 32x32; values encode
    /// (col, row, col+row) so we can spot-check sample/read_window.
    fn write_fixture() -> (NamedTempFile, Bbox, Crs, u32, u32, u32) {
        let width: u32 = 32;
        let height: u32 = 32;
        let bands: u32 = 3;
        let mut data: Vec<u8> = Vec::with_capacity((width * height * bands) as usize);
        for row in 0..height {
            for col in 0..width {
                data.push(col as u8 * 8); // R
                data.push(row as u8 * 8); // G
                data.push((row as u8).wrapping_add(col as u8).wrapping_mul(4)); // B
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
        let crs = Crs::new("EPSG:4326");
        let bbox = Bbox::from([0.0, 0.0, 1.0, 1.0]);
        let georef = GeoRef {
            crs: crs.clone(),
            bbox,
            nodata: None,
        };
        let bytes = encode(
            &buffer,
            Some(&georef),
            OutputFormat::GeoTiff,
            &EncodeOpts::defaults_for(OutputFormat::GeoTiff),
        )
        .expect("encode fixture GeoTIFF");

        let mut tmp = tempfile::Builder::new()
            .prefix("kwms-gdal-fixture-")
            .suffix(".tif")
            .tempfile()
            .expect("create tempfile");
        tmp.write_all(&bytes).expect("write fixture tempfile");
        tmp.flush().expect("flush fixture tempfile");
        (tmp, bbox, crs, width, height, bands)
    }

    #[test]
    fn open_succeeds_for_valid_fixture_and_fails_for_bad_path() {
        let (tmp, _, _, _, _, _) = write_fixture();
        let cfg = SourceConfig {
            uri: tmp.path().to_string_lossy().into_owned(),
            credentials: None,
        };
        let src = GdalSource::open(&cfg).expect("open valid fixture");
        assert_eq!(src.metadata().band_count, 3);

        let bad = SourceConfig {
            uri: "/this/path/does/not/exist/kwms-nope.tif".into(),
            credentials: None,
        };
        let err = match GdalSource::open(&bad) {
            Ok(_) => panic!("open should fail on bad path"),
            Err(e) => e,
        };
        // Error chain should mention the URI somewhere so operators can
        // grep their logs.
        let msg = format!("{err:#}");
        assert!(
            msg.contains("kwms-nope.tif"),
            "expected error to mention bad URI, got: {msg}"
        );
    }

    #[test]
    fn metadata_reports_crs_bbox_and_dimensions() {
        let (tmp, bbox, crs, _w, _h, bands) = write_fixture();
        let cfg = SourceConfig {
            uri: tmp.path().to_string_lossy().into_owned(),
            credentials: None,
        };
        let src = GdalSource::open(&cfg).expect("open");
        let md = src.metadata();
        assert_eq!(md.band_count, bands);
        assert_eq!(md.pixel_type, PixelType::U8);

        // GDAL may round-trip EPSG:4326 as `EPSG:4326` (from auth_code) or
        // as a WKT string; either way the CRS should describe WGS 84.
        let native = md.native_crs.as_str();
        assert!(
            native.contains("4326") || native.contains("WGS 84"),
            "expected native CRS to mention WGS 84 / 4326, got: {native}"
        );

        // Bbox should be (approximately) the bbox we wrote.
        let eps = 1e-9;
        assert!((md.native_bbox.minx - bbox.minx).abs() < eps, "{:?}", md.native_bbox);
        assert!((md.native_bbox.maxx - bbox.maxx).abs() < eps, "{:?}", md.native_bbox);
        assert!((md.native_bbox.miny - bbox.miny).abs() < eps, "{:?}", md.native_bbox);
        assert!((md.native_bbox.maxy - bbox.maxy).abs() < eps, "{:?}", md.native_bbox);

        // Native resolution: 1 degree / 32 pixels = 0.03125
        assert!((md.native_resolution - (1.0_f64 / 32.0)).abs() < 1e-9);

        let _ = crs; // referenced in earlier branch only.
    }

    #[tokio::test]
    async fn read_window_native_crs_returns_full_buffer() {
        let (tmp, bbox, crs, width, height, bands) = write_fixture();
        let cfg = SourceConfig {
            uri: tmp.path().to_string_lossy().into_owned(),
            credentials: None,
        };
        let src = GdalSource::open(&cfg).expect("open");
        let req = WindowRequest {
            bbox,
            crs,
            width,
            height,
            resampling: ResampleAlg::NearestNeighbour,
            nodata: None,
        };
        let pixels = src.read_window(req).await.expect("read_window");
        assert_eq!(pixels.width, width);
        assert_eq!(pixels.height, height);
        assert_eq!(pixels.bands, bands);
        assert_eq!(pixels.pixel_type, PixelType::U8);
        let expected_bytes = (width as usize) * (height as usize) * (bands as usize) * 1;
        assert_eq!(pixels.data.len(), expected_bytes);
    }

    #[tokio::test]
    async fn read_window_different_crs_triggers_warping() {
        let (tmp, _bbox, _crs, _w, _h, bands) = write_fixture();
        let cfg = SourceConfig {
            uri: tmp.path().to_string_lossy().into_owned(),
            credentials: None,
        };
        let src = GdalSource::open(&cfg).expect("open");

        // Pick a bbox inside the fixture's footprint, transformed to a
        // different CRS. Using EPSG:3857 (Web Mercator) keeps the example
        // realistic; we pick a destination size that's not the source
        // pixel size so we exercise the resample path too.
        let target_crs = Crs::new("EPSG:3857");
        // Roughly a sub-square inside the [0,0]-[1,1] WGS84 fixture,
        // converted to mercator metres.
        let target_bbox = Bbox::from([10_000.0, 10_000.0, 100_000.0, 100_000.0]);
        let target_w = 16u32;
        let target_h = 16u32;
        let req = WindowRequest {
            bbox: target_bbox,
            crs: target_crs,
            width: target_w,
            height: target_h,
            resampling: ResampleAlg::Bilinear,
            nodata: None,
        };
        let pixels = src.read_window(req).await.expect("warped read_window");
        assert_eq!(pixels.width, target_w);
        assert_eq!(pixels.height, target_h);
        assert_eq!(pixels.bands, bands);
        let expected_bytes = (target_w as usize) * (target_h as usize) * (bands as usize) * 1;
        assert_eq!(pixels.data.len(), expected_bytes);
    }

    #[tokio::test]
    async fn sample_returns_one_value_per_band_at_centre() {
        let (tmp, bbox, crs, _w, _h, bands) = write_fixture();
        let cfg = SourceConfig {
            uri: tmp.path().to_string_lossy().into_owned(),
            credentials: None,
        };
        let src = GdalSource::open(&cfg).expect("open");
        let centre = Coord {
            x: (bbox.minx + bbox.maxx) / 2.0,
            y: (bbox.miny + bbox.maxy) / 2.0,
        };
        let values = src.sample(centre, crs).await.expect("sample");
        assert_eq!(values.len(), bands as usize);
        for (i, pv) in values.iter().enumerate() {
            assert_eq!(pv.band, (i + 1) as u32);
            assert!(pv.value.is_finite(), "non-finite sample: {pv:?}");
        }
    }
}
