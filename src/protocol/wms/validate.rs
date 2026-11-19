//! WMS request validation — stage 2 (rules against the catalog).
//!
//! | Rule | Error (locator) |
//! |---|---|
//! | GetFeatureInfo is not implemented | `OperationNotSupported` (REQUEST) |
//! | `LAYERS` names exactly one layer (LayerLimit 1) | `InvalidParameterValue` (LAYERS) |
//! | that layer is configured | `LayerNotDefined` (LAYERS) |
//! | `STYLES` is empty, or one entry per layer that is empty or `default` | `StyleNotDefined` (STYLES) |
//! | `CRS`/`SRS` is advertised for the layer (any OGC spelling) | `InvalidCRS` (CRS / SRS) |
//! | `WIDTH`, `HEIGHT` ≤ `server.max_request_pixels` | `InvalidParameterValue` (WIDTH / HEIGHT) |
//! | BBOX extent / size is finite, and the pixel size is ≥ the grid's finest resolution ÷ [`MAX_OVERSAMPLING`] and well above float precision | `InvalidParameterValue` (BBOX) |
//! | `FORMAT` is offered by the layer | `InvalidFormat` (FORMAT) |
//!
//! The BBOX is converted to x/y using the axis order the catalog recorded
//! for the CRS at startup (WMS 1.3.0 uses the CRS's authority order). A
//! BBOX that misses the grid entirely is valid: it renders a blank image.
//!
//! Everything a [`WmsGetMap`] hands downstream is a catalog entry — the CRS
//! is the config's own token, never the client's string.

use super::request::{GetMapRequest, WmsRequest, WmsVersion};
use crate::crs::{AxisOrder, Bbox};
use crate::encode::OutputFormat;
use crate::protocol::catalog::{Catalog, CatalogCrs, CatalogGrid, CatalogLayer};
use crate::protocol::error::{OgcCode, OgcError, Param};

/// A validated WMS request.
pub enum WmsQuery<'c> {
    GetCapabilities { version: WmsVersion },
    GetMap(WmsGetMap<'c>),
}

/// A validated GetMap. Fields are private: only [`validate`] builds one.
pub struct WmsGetMap<'c> {
    version: WmsVersion,
    layer: &'c CatalogLayer,
    grid: &'c CatalogGrid,
    crs: &'c CatalogCrs,
    bbox: Bbox,
    width: u32,
    height: u32,
    format: OutputFormat,
    transparent: bool,
    bgcolor: Option<u32>,
    intersects_grid: bool,
}

impl<'c> WmsGetMap<'c> {
    pub fn version(&self) -> WmsVersion {
        self.version
    }
    pub fn layer(&self) -> &'c CatalogLayer {
        self.layer
    }
    /// The layer's grid in the requested CRS (first in config order).
    pub fn grid(&self) -> &'c CatalogGrid {
        self.grid
    }
    /// The requested CRS as the catalog knows it (config token).
    pub fn crs(&self) -> &'c CatalogCrs {
        self.crs
    }
    /// Requested area in x/y order, in the CRS's units.
    pub fn bbox(&self) -> Bbox {
        self.bbox
    }
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    pub fn format(&self) -> OutputFormat {
        self.format
    }
    pub fn transparent(&self) -> bool {
        self.transparent
    }
    pub fn bgcolor(&self) -> Option<u32> {
        self.bgcolor
    }
    /// False when the BBOX lies entirely outside the grid's extent.
    pub fn intersects_grid(&self) -> bool {
        self.intersects_grid
    }
}

pub fn validate(req: WmsRequest, catalog: &Catalog) -> Result<WmsQuery<'_>, OgcError> {
    match req {
        WmsRequest::GetCapabilities { version } => Ok(WmsQuery::GetCapabilities { version }),
        WmsRequest::GetMap(m) => validate_get_map(m, catalog).map(WmsQuery::GetMap),
        WmsRequest::GetFeatureInfo { .. } => Err(OgcError::new(
            OgcCode::OperationNotSupported,
            Some(Param::Request),
            "GetFeatureInfo is not supported",
        )),
    }
}

fn validate_get_map(m: GetMapRequest, catalog: &Catalog) -> Result<WmsGetMap<'_>, OgcError> {
    // LAYERS: exactly one, and it exists.
    let [name] = m.layers.as_slice() else {
        return Err(OgcError::invalid(
            Param::Layers,
            "exactly one layer per request is supported",
        ));
    };
    let layer = catalog.layer(name).ok_or(OgcError::new(
        OgcCode::LayerNotDefined,
        Some(Param::Layers),
        "LAYERS is not a layer of this service",
    ))?;

    // STYLES: empty, or one default style per layer.
    if !m.styles.is_empty() {
        if m.styles.len() != m.layers.len() {
            return Err(OgcError::new(
                OgcCode::StyleNotDefined,
                Some(Param::Styles),
                "STYLES must list one style per layer",
            ));
        }
        if m.styles.iter().any(|s| !(s.is_empty() || s == "default")) {
            return Err(OgcError::new(
                OgcCode::StyleNotDefined,
                Some(Param::Styles),
                "STYLES must be empty or default",
            ));
        }
    }

    // CRS: advertised for this layer — resolved by the catalog, no GDAL.
    let unsupported = OgcError::new(
        OgcCode::InvalidCrs,
        Some(m.version.crs_param()),
        "the CRS is not offered for this layer",
    );
    let crs_id = catalog.resolve_crs(&m.crs).ok_or(unsupported)?;
    let grid = layer.grid_for_crs(crs_id).ok_or(unsupported)?;
    let crs = catalog.crs(crs_id);

    // BBOX: wire order → x/y. Only 1.3.0 follows the CRS's axis order.
    let [a, b, c, d] = m.bbox;
    let swap = m.version == WmsVersion::V1_3_0 && crs.axis_order() == AxisOrder::YX;
    let bbox = if swap {
        Bbox::from([b, a, d, c])
    } else {
        Bbox::from([a, b, c, d])
    };

    // Size limit (advertised as MaxWidth/MaxHeight).
    let [max_w, max_h] = catalog.max_request_pixels();
    if m.width > max_w || m.height > max_h {
        let param = if m.width > max_w { Param::Width } else { Param::Height };
        return Err(OgcError::invalid(param, "WIDTH/HEIGHT exceed the maximum image size"));
    }

    check_bbox_geometry(&bbox, (m.width, m.height), grid)?;

    // FORMAT offered by the layer.
    if !layer.offers(m.format) {
        return Err(OgcError::new(
            OgcCode::InvalidFormat,
            Some(Param::Format),
            "FORMAT is not offered for this layer",
        ));
    }

    let extent = grid.grid().extent();
    let intersects_grid =
        bbox.minx < extent.maxx && extent.minx < bbox.maxx && bbox.miny < extent.maxy && extent.miny < bbox.maxy;

    Ok(WmsGetMap {
        version: m.version,
        layer,
        grid,
        crs,
        bbox,
        width: m.width,
        height: m.height,
        format: m.format,
        transparent: m.transparent,
        bgcolor: m.bgcolor,
        intersects_grid,
    })
}

/// How far beyond the grid's finest level a GetMap may zoom in (pixel size
/// at least the finest resolution divided by this).
pub const MAX_OVERSAMPLING: f64 = 1024.0;

/// Pixel sizes must exceed the coordinates' float precision by this
/// factor (2^16 ULPs), so neighbouring pixel edges stay distinct.
const MIN_PIXEL_ULPS: f64 = 65536.0;

/// The output pixel grid a BBOX + size defines must be one GDAL can
/// georeference faithfully. Every coordinate is already finite (stage 1),
/// but their difference can overflow (`-1e308,…,1e308`), and a tiny extent
/// gives pixels so small that GDAL can't invert the geotransform or rounds
/// every pixel onto the same coordinate (a silently blank image).
fn check_bbox_geometry(bbox: &Bbox, (width, height): (u32, u32), grid: &CatalogGrid) -> Result<(), OgcError> {
    let res_x = (bbox.maxx - bbox.minx) / f64::from(width);
    let res_y = (bbox.maxy - bbox.miny) / f64::from(height);
    if !(res_x.is_finite() && res_y.is_finite()) {
        return Err(OgcError::invalid(Param::Bbox, "BBOX extent is too large"));
    }
    let g = grid.grid();
    let finest = g.resolution(g.level_count().saturating_sub(1));
    let magnitude = [bbox.minx, bbox.miny, bbox.maxx, bbox.maxy]
        .iter()
        .fold(0.0f64, |m, v| m.max(v.abs()));
    let min_res = (finest / MAX_OVERSAMPLING).max(magnitude * f64::EPSILON * MIN_PIXEL_ULPS);
    // Also rejects a NaN `min_res` (a degenerate grid), never accepts it.
    if !(res_x >= min_res && res_y >= min_res) {
        return Err(OgcError::invalid(
            Param::Bbox,
            "BBOX is too small for WIDTH/HEIGHT: pixels finer than the layer can be rendered",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, LayerConfig};
    use crate::protocol::wms::request::parse;

    fn catalog() -> Catalog {
        let layer = |name: &str, grids: &str| -> LayerConfig {
            serde_yaml::from_str(&format!(
                "name: {name}\ntitle: {name}\ngrids: [{grids}]\nsources: [{{uri: memory://{name}}}]\nformats: [jpeg]\n"
            ))
            .unwrap()
        };
        let config = Config {
            layers: vec![
                layer("ortho", "rd_new_quad, webmercator, global_geodetic"),
                layer("laea", "etrs89_laea_quad"),
            ],
            ..Default::default()
        };
        Catalog::build(&config).unwrap()
    }

    fn map_query(version: &str, crs: &str, bbox: &str, extra: &str) -> String {
        let key = if version == "1.1.1" { "SRS" } else { "CRS" };
        format!(
            "SERVICE=WMS&VERSION={version}&REQUEST=GetMap&LAYERS=ortho&STYLES=&{key}={crs}\
             &BBOX={bbox}&WIDTH=256&HEIGHT=256&FORMAT=image/jpeg{extra}"
        )
    }

    fn get_map<'c>(cat: &'c Catalog, q: &str) -> Result<WmsGetMap<'c>, OgcError> {
        match validate(parse(q)?, cat)? {
            WmsQuery::GetMap(m) => Ok(m),
            WmsQuery::GetCapabilities { .. } => panic!("expected GetMap"),
        }
    }

    #[test]
    fn resolves_crs_and_grid_through_the_catalog() {
        let cat = catalog();
        let m = get_map(
            &cat,
            &map_query("1.3.0", "urn:ogc:def:crs:EPSG::3857", "0,0,1000,1000", ""),
        )
        .unwrap();
        assert_eq!(m.crs().crs().as_str(), "EPSG:3857", "downstream sees the config token");
        assert_eq!(m.grid().name(), "webmercator");
        assert!(m.intersects_grid());
        let m = get_map(&cat, &map_query("1.3.0", "epsg:28992", "0,300000,1000,301000", "")).unwrap();
        assert_eq!(m.grid().name(), "rd_new_quad");
    }

    #[test]
    fn axis_order_from_the_catalog() {
        let cat = catalog();
        // 1.3.0 + EPSG:4326 → wire is lat,lon.
        let m = get_map(&cat, &map_query("1.3.0", "EPSG:4326", "50,3,54,7", "")).unwrap();
        assert_eq!(m.bbox(), Bbox::from([3.0, 50.0, 7.0, 54.0]));
        // 1.1.1 is always x,y.
        let m = get_map(&cat, &map_query("1.1.1", "EPSG:4326", "3,50,7,54", "")).unwrap();
        assert_eq!(m.bbox(), Bbox::from([3.0, 50.0, 7.0, 54.0]));
        // Projected but northing-first (EPSG:3035) swaps too.
        let q = map_query("1.3.0", "EPSG:3035", "3000000,4000000,3100000,4100000", "")
            .replace("LAYERS=ortho", "LAYERS=laea");
        assert_eq!(
            get_map(&cat, &q).unwrap().bbox(),
            Bbox::from([4_000_000.0, 3_000_000.0, 4_100_000.0, 3_100_000.0])
        );
    }

    #[test]
    fn each_rule_has_its_error() {
        let cat = catalog();
        let code = |q: String| {
            let e = get_map(&cat, &q).err().expect("must be rejected");
            (e.code, e.locator_name())
        };
        let ok = map_query("1.3.0", "EPSG:3857", "0,0,1000,1000", "");
        assert_eq!(
            code(ok.replace("LAYERS=ortho", "LAYERS=ortho,laea")),
            (OgcCode::InvalidParameterValue, Some("LAYERS"))
        );
        assert_eq!(
            code(ok.replace("LAYERS=ortho", "LAYERS=nope")),
            (OgcCode::LayerNotDefined, Some("LAYERS"))
        );
        assert_eq!(
            code(ok.replace("STYLES=", "STYLES=fancy")),
            (OgcCode::StyleNotDefined, Some("STYLES"))
        );
        assert_eq!(code(ok.replace("STYLES=", "STYLES=,")).0, OgcCode::StyleNotDefined);
        assert_eq!(
            code(ok.replace("EPSG:3857", "EPSG:3035")),
            (OgcCode::InvalidCrs, Some("CRS")),
            "advertised, but not for this layer"
        );
        assert_eq!(
            code(ok.replace("EPSG:3857", "EPSG:2154")).0,
            OgcCode::InvalidCrs,
            "valid EPSG, not advertised"
        );
        assert_eq!(
            code(ok.replace("EPSG:3857", "CRS:84")).0,
            OgcCode::InvalidCrs,
            "not advertised"
        );
        for hostile in [
            "/etc/passwd",
            "http://169.254.169.254/x",
            "%2Fvsicurl%2Fhttp%3A%2F%2Fx",
            "+proj=merc",
        ] {
            assert_eq!(
                code(ok.replace("EPSG:3857", hostile)).0,
                OgcCode::InvalidCrs,
                "{hostile}"
            );
        }
        assert_eq!(
            code(map_query("1.1.1", "EPSG:9999", "0,0,1,1", "")),
            (OgcCode::InvalidCrs, Some("SRS"))
        );
        assert_eq!(
            code(ok.replace("WIDTH=256", "WIDTH=2501")),
            (OgcCode::InvalidParameterValue, Some("WIDTH"))
        );
        assert_eq!(code(ok.replace("HEIGHT=256", "HEIGHT=4000")).1, Some("HEIGHT"));
        assert_eq!(code(ok.replace("image/jpeg", "image/png")).0, OgcCode::InvalidFormat);
        let e = validate(parse("REQUEST=GetFeatureInfo&VERSION=1.3.0").unwrap(), &cat)
            .err()
            .unwrap();
        assert_eq!(e.code, OgcCode::OperationNotSupported);
    }

    #[test]
    fn bbox_geometry_gdal_cannot_georeference_is_rejected() {
        let cat = catalog();
        let bbox_err = |bbox: &str| {
            let e = get_map(&cat, &map_query("1.3.0", "EPSG:3857", bbox, "")).err();
            e.map(|e| (e.code, e.locator_name()))
        };
        let rejected = Some((OgcCode::InvalidParameterValue, Some("BBOX")));
        // Found by the hostile-BBOX probe: 200 with a NaN/inf geotransform,
        // a 500 from GDAL, and silently blank images.
        assert_eq!(bbox_err("-1e308,-1e308,1e308,1e308"), rejected, "extent overflows");
        assert_eq!(bbox_err("-1.7e308,0,1.7e308,1000"), rejected, "x extent overflows");
        assert_eq!(bbox_err("0,0,5e-324,5e-324"), rejected, "subnormal extent");
        assert_eq!(bbox_err("0,0,1e-300,1e-300"), rejected);
        assert_eq!(bbox_err("0,0,1e-9,1e-9"), rejected);
        assert_eq!(
            bbox_err("10000000,10000000,10000000.000000002,10000000.000000002"),
            rejected
        );
        assert_eq!(bbox_err("0,-20037508,1e-300,20037508"), rejected, "one axis too thin");
        // Still fine: zoomed in a lot, huge but finite, far outside the grid.
        assert_eq!(bbox_err("0,0,1,1"), None);
        assert_eq!(bbox_err("-1e300,-1e300,1e300,1e300"), None);
        assert_eq!(bbox_err("1e9,1e9,2e9,2e9"), None);
        // In 1.3.0 + EPSG:4326 the check runs on the swapped (x/y) values.
        let q = map_query("1.3.0", "EPSG:4326", "52,4,52.0000000001,4.0000000001", "");
        assert_eq!(get_map(&cat, &q).err().map(|e| e.locator_name()), Some(Some("BBOX")));
    }

    #[test]
    fn bbox_outside_the_grid_is_valid_but_flagged() {
        let cat = catalog();
        let m = get_map(&cat, &map_query("1.3.0", "EPSG:3857", "1e9,1e9,2e9,2e9", "")).unwrap();
        assert!(!m.intersects_grid());
    }
}
