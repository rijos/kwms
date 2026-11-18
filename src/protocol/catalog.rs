//! `Catalog` — everything a request is allowed to refer to, built once from
//! the validated config at startup.
//!
//! The protocol validators resolve request parameters *against* the catalog:
//! a layer name becomes a [`CatalogLayer`], a CRS spelling becomes a
//! [`CrsId`], a TileMatrixSet becomes a [`CatalogGrid`]. Whatever a
//! validated request carries downstream is therefore a catalog entry, and
//! every CRS that reaches GDAL is the config's own token — never a string a
//! client sent.
//!
//! GDAL is consulted only here, at build time: each configured CRS is parsed
//! (a CRS GDAL rejects stops startup) and its axis order recorded. At request
//! time, [`Catalog::resolve_crs`] is a plain string-table lookup.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};

use crate::config::{Config, LayerConfig};
use crate::crs::{identify_crs, native_axis_order, AxisOrder, Crs};
use crate::encode::OutputFormat;
use crate::tile::grid::{lookup_grid, tile_matrix_set_name, well_known_grid, TileGrid};

/// Index of a CRS in the catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CrsId(usize);

/// A CRS the server advertises.
#[derive(Debug, Clone)]
pub struct CatalogCrs {
    /// The config's token for this CRS — the only form GDAL ever sees.
    crs: Crs,
    /// Authority axis order, as GDAL reported it at startup (decides the
    /// WMS 1.3.0 BBOX layout).
    axis_order: AxisOrder,
}

impl CatalogCrs {
    pub fn crs(&self) -> &Crs {
        &self.crs
    }
    pub fn axis_order(&self) -> AxisOrder {
        self.axis_order
    }
}

/// A grid a layer is published on.
pub struct CatalogGrid {
    /// Config-level grid name (`webmercator`, or a custom grid's key).
    name: String,
    /// WMTS TileMatrixSet identifier.
    tile_matrix_set: String,
    grid: Arc<dyn TileGrid>,
    crs: CrsId,
}

impl CatalogGrid {
    pub fn name(&self) -> &str {
        &self.name
    }
    pub fn tile_matrix_set(&self) -> &str {
        &self.tile_matrix_set
    }
    pub fn grid(&self) -> &Arc<dyn TileGrid> {
        &self.grid
    }
    pub fn crs(&self) -> CrsId {
        self.crs
    }
}

/// A configured layer and the grids it is published on, in config order.
pub struct CatalogLayer {
    config: LayerConfig,
    grids: Vec<CatalogGrid>,
}

impl CatalogLayer {
    pub fn name(&self) -> &str {
        &self.config.name
    }
    pub fn config(&self) -> &LayerConfig {
        &self.config
    }
    pub fn grids(&self) -> &[CatalogGrid] {
        &self.grids
    }
    /// Whether the layer is offered in `format`.
    pub fn offers(&self, format: OutputFormat) -> bool {
        self.config.formats.contains(&format)
    }
    /// The CRSes the layer is advertised in (one per distinct grid CRS).
    pub fn crses(&self) -> Vec<CrsId> {
        let mut out = Vec::new();
        for g in &self.grids {
            if !out.contains(&g.crs) {
                out.push(g.crs);
            }
        }
        out
    }
    /// The first of the layer's grids in `crs` (config order).
    pub fn grid_for_crs(&self, crs: CrsId) -> Option<&CatalogGrid> {
        self.grids.iter().find(|g| g.crs == crs)
    }
    /// The layer's grid published as `tile_matrix_set`: the advertised
    /// identifier exactly, or a registered legacy alias of a built-in grid.
    pub fn grid_for_tile_matrix_set(&self, tile_matrix_set: &str) -> Option<&CatalogGrid> {
        self.grids.iter().find(|g| {
            g.tile_matrix_set == tile_matrix_set
                || well_known_grid(&g.name)
                    .is_some_and(|w| w.tile_matrix_set_aliases.contains(&tile_matrix_set))
        })
    }
    /// The layer's primary grid (the first listed) — what TMS serves.
    pub fn primary_grid(&self) -> &CatalogGrid {
        &self.grids[0]
    }
}

/// Everything requests may refer to. See the module docs.
pub struct Catalog {
    layers: Vec<CatalogLayer>,
    crses: Vec<CatalogCrs>,
    /// Largest WMS GetMap output, `[width, height]` (`server.max_request_pixels`).
    max_request_pixels: [u32; 2],
    /// Normalised spelling → CRS (see [`normalize_crs_spelling`]).
    crs_spellings: HashMap<String, CrsId>,
}

impl Catalog {
    /// Build from a config that already passed `config::validate`.
    pub fn build(config: &Config) -> Result<Self> {
        let mut catalog = Catalog {
            layers: Vec::new(),
            crses: Vec::new(),
            max_request_pixels: config.server.max_request_pixels,
            crs_spellings: HashMap::new(),
        };
        for layer in &config.layers {
            if layer.grids.is_empty() {
                return Err(anyhow!("layer `{}` has no grids", layer.name));
            }
            let mut grids = Vec::with_capacity(layer.grids.len());
            for name in &layer.grids {
                let grid = lookup_grid(name, &config.grids).ok_or_else(|| {
                    anyhow!("layer `{}` references unknown grid `{name}`", layer.name)
                })?;
                let crs = catalog
                    .intern_crs(grid.crs())
                    .with_context(|| format!("grid `{name}` of layer `{}`", layer.name))?;
                grids.push(CatalogGrid {
                    name: name.clone(),
                    tile_matrix_set: tile_matrix_set_name(name).to_string(),
                    grid,
                    crs,
                });
            }
            catalog.layers.push(CatalogLayer {
                config: layer.clone(),
                grids,
            });
        }
        Ok(catalog)
    }

    /// Register a configured CRS (deduplicated by canonical identity),
    /// validating it with GDAL. Startup only.
    fn intern_crs(&mut self, crs: &Crs) -> Result<CrsId> {
        // Canonical key: the EPSG/OGC code if the token spells one, else what
        // GDAL identifies it as (e.g. a WKT definition), else the token.
        let key = match normalize_crs_spelling(crs.as_str()) {
            Some(k) => k,
            None => {
                let identified = identify_crs(crs)
                    .with_context(|| format!("GDAL cannot use configured CRS '{crs}'"))?;
                normalize_crs_spelling(identified.as_str()).unwrap_or_else(|| crs.as_str().to_string())
            }
        };
        if let Some(&id) = self.crs_spellings.get(&key) {
            self.crs_spellings
                .entry(crs.as_str().to_string())
                .or_insert(id);
            return Ok(id);
        }
        let axis_order = native_axis_order(crs)
            .with_context(|| format!("GDAL cannot use configured CRS '{crs}'"))?;
        let id = CrsId(self.crses.len());
        self.crses.push(CatalogCrs {
            crs: crs.clone(),
            axis_order,
        });
        self.crs_spellings.insert(key, id);
        // The exact config token is also a valid spelling.
        self.crs_spellings
            .entry(crs.as_str().to_string())
            .or_insert(id);
        Ok(id)
    }

    pub fn layers(&self) -> &[CatalogLayer] {
        &self.layers
    }

    pub fn layer(&self, name: &str) -> Option<&CatalogLayer> {
        self.layers.iter().find(|l| l.config.name == name)
    }

    /// Largest WMS GetMap output, `[width, height]`.
    pub fn max_request_pixels(&self) -> [u32; 2] {
        self.max_request_pixels
    }

    pub fn crs(&self, id: CrsId) -> &CatalogCrs {
        &self.crses[id.0]
    }

    /// Resolve a CRS as a client spelled it. Pure string matching — GDAL is
    /// never called with request input. `None` means the server does not
    /// advertise that CRS at all.
    pub fn resolve_crs(&self, spelling: &str) -> Option<CrsId> {
        let key = normalize_crs_spelling(spelling).unwrap_or_else(|| spelling.trim().to_string());
        self.crs_spellings.get(&key).copied()
    }
}

/// Canonical form of the CRS spellings OGC clients use, or `None` if
/// `spelling` is none of them:
///
/// | Spelling | Canonical |
/// |---|---|
/// | `EPSG:3857`, `epsg:3857` | `EPSG:3857` |
/// | `urn:ogc:def:crs:EPSG::3857`, `urn:ogc:def:crs:EPSG:6.18:3857` | `EPSG:3857` |
/// | `http(s)://www.opengis.net/def/crs/EPSG/0/3857` | `EPSG:3857` |
/// | `CRS:84`, `urn:ogc:def:crs:OGC:1.3:CRS84`, `http(s)://www.opengis.net/def/crs/OGC/1.3/CRS84` | `CRS:84` |
///
/// (likewise `CRS:83` and `CRS:27`). Codes must be 1–9 decimal digits;
/// leading zeros are normalised away.
pub fn normalize_crs_spelling(spelling: &str) -> Option<String> {
    let s = spelling.trim();
    let lower = s.to_ascii_lowercase();
    let epsg = |code: &str| -> Option<String> {
        if code.is_empty() || code.len() > 9 || !code.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        Some(format!("EPSG:{}", code.parse::<u32>().ok()?))
    };
    let ogc = |code: &str| -> Option<String> {
        matches!(code, "84" | "83" | "27").then(|| format!("CRS:{code}"))
    };
    if let Some(code) = lower.strip_prefix("epsg:") {
        return epsg(code);
    }
    if let Some(code) = lower.strip_prefix("crs:") {
        return ogc(code);
    }
    if let Some(rest) = lower.strip_prefix("urn:ogc:def:crs:") {
        // urn:ogc:def:crs:{authority}:{version}:{code}; version may be empty.
        let mut parts = rest.splitn(3, ':');
        let (auth, version, code) = (parts.next()?, parts.next()?, parts.next()?);
        if !version.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
            return None;
        }
        return match auth {
            "epsg" => epsg(code),
            "ogc" => ogc(code.strip_prefix("crs")?),
            _ => None,
        };
    }
    let path = lower
        .strip_prefix("http://www.opengis.net/def/crs/")
        .or_else(|| lower.strip_prefix("https://www.opengis.net/def/crs/"))?;
    let mut parts = path.splitn(3, '/');
    let (auth, version, code) = (parts.next()?, parts.next()?, parts.next()?);
    match (auth, version) {
        ("epsg", "0") => epsg(code),
        ("ogc", "1.3") => ogc(code.strip_prefix("crs")?),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::GridConfig;

    fn layer(name: &str, grids: &[&str]) -> LayerConfig {
        let yaml = format!(
            "name: {name}\ntitle: {name}\ngrids: [{}]\nsources: [{{uri: memory://{name}}}]\nformats: [png]\n",
            grids.join(", ")
        );
        serde_yaml::from_str(&yaml).unwrap()
    }

    fn catalog(layers: Vec<LayerConfig>) -> Catalog {
        Catalog::build(&Config {
            layers,
            ..Default::default()
        })
        .unwrap()
    }

    #[test]
    fn normalizes_ogc_crs_spellings() {
        for s in [
            "EPSG:3857",
            "epsg:3857",
            " EPSG:03857 ",
            "urn:ogc:def:crs:EPSG::3857",
            "URN:OGC:DEF:CRS:EPSG:6.18:3857",
            "http://www.opengis.net/def/crs/EPSG/0/3857",
            "https://www.opengis.net/def/crs/EPSG/0/3857",
        ] {
            assert_eq!(
                normalize_crs_spelling(s).as_deref(),
                Some("EPSG:3857"),
                "{s}"
            );
        }
        for s in [
            "CRS:84",
            "crs:84",
            "urn:ogc:def:crs:OGC:1.3:CRS84",
            "http://www.opengis.net/def/crs/OGC/1.3/CRS84",
        ] {
            assert_eq!(normalize_crs_spelling(s).as_deref(), Some("CRS:84"), "{s}");
        }
        for s in [
            "EPSG:",
            "EPSG:12345678901",
            "EPSG:38x57",
            "CRS:85",
            "urn:ogc:def:crs:EPSG:x:3857",
            "http://evil.example/def/crs/EPSG/0/3857",
            "/etc/passwd",
            "+proj=merc",
        ] {
            assert_eq!(normalize_crs_spelling(s), None, "{s}");
        }
    }

    #[test]
    fn resolves_only_advertised_crses_and_returns_config_tokens() {
        let cat = catalog(vec![
            layer("a", &["webmercator", "rd_new_quad"]),
            layer("b", &["webmercator"]),
        ]);
        let wm = cat
            .resolve_crs("urn:ogc:def:crs:EPSG::3857")
            .expect("3857 advertised");
        assert_eq!(cat.resolve_crs("epsg:3857"), Some(wm));
        assert_eq!(cat.crs(wm).crs().as_str(), "EPSG:3857");
        assert_eq!(cat.crs(wm).axis_order(), AxisOrder::XY);
        assert!(cat.resolve_crs("EPSG:28992").is_some());
        // Valid EPSG codes the server doesn't advertise are unknown.
        assert_eq!(cat.resolve_crs("EPSG:4326"), None);
        assert_eq!(cat.resolve_crs("/etc/passwd"), None);
        // The two layers share one CRS entry.
        assert_eq!(cat.layer("a").unwrap().crses().len(), 2);
        assert_eq!(cat.layer("b").unwrap().crses(), vec![wm]);
    }

    #[test]
    fn records_gdal_axis_order_at_build_time() {
        let cat = catalog(vec![
            layer("geo", &["global_geodetic"]),
            layer("laea", &["etrs89_laea_quad"]),
        ]);
        let geo = cat.resolve_crs("EPSG:4326").unwrap();
        let laea = cat.resolve_crs("EPSG:3035").unwrap();
        assert_eq!(cat.crs(geo).axis_order(), AxisOrder::YX);
        assert_eq!(cat.crs(laea).axis_order(), AxisOrder::YX);
        // CRS:84 is a different CRS (lon/lat) and is not advertised here.
        assert_eq!(cat.resolve_crs("CRS:84"), None);
    }

    #[test]
    fn layer_grid_lookups() {
        let cat = catalog(vec![layer("a", &["webmercator", "global_geodetic"])]);
        let a = cat.layer("a").unwrap();
        assert_eq!(a.primary_grid().name(), "webmercator");
        assert_eq!(
            a.grid_for_tile_matrix_set("GoogleMapsCompatible")
                .unwrap()
                .name(),
            "webmercator"
        );
        assert_eq!(
            a.grid_for_tile_matrix_set("GoogleCRS84Quad")
                .unwrap()
                .name(),
            "global_geodetic"
        );
        assert!(a.grid_for_tile_matrix_set("NetherlandsRDNewQuad").is_none());
        // Config grid names are not TileMatrixSet identifiers.
        assert!(a.grid_for_tile_matrix_set("webmercator").is_none());
        let geo = cat.resolve_crs("EPSG:4326").unwrap();
        assert_eq!(
            a.grid_for_crs(geo).unwrap().tile_matrix_set(),
            "WorldCRS84Quad"
        );
        assert!(a.offers(OutputFormat::Png) && !a.offers(OutputFormat::GeoTiff));
        assert!(cat.layer("nope").is_none());
    }

    #[test]
    fn custom_grid_crs_written_as_wkt_resolves_by_its_epsg_code() {
        let wkt = gdal::spatial_ref::SpatialRef::from_epsg(31370)
            .unwrap()
            .to_wkt()
            .unwrap();
        let mut config = Config {
            layers: vec![layer("be", &["lambert72"])],
            ..Default::default()
        };
        config.grids.insert(
            "lambert72".into(),
            GridConfig {
                crs: Crs::new(wkt.clone()),
                top_left: [0.0, 300_000.0],
                tile_size: [256, 256],
                base_resolution: 1024.0,
                origin: Default::default(),
            },
        );
        let cat = Catalog::build(&config).unwrap();
        let id = cat
            .resolve_crs("EPSG:31370")
            .expect("identified via GDAL at build time");
        assert_eq!(
            cat.crs(id).crs().as_str(),
            wkt,
            "downstream sees the config token"
        );
    }

    #[test]
    fn unusable_configured_crs_fails_the_build() {
        let mut config = Config {
            layers: vec![layer("x", &["bad"])],
            ..Default::default()
        };
        config.grids.insert(
            "bad".into(),
            GridConfig {
                crs: Crs::new("EPSG:99999"),
                top_left: [0.0, 0.0],
                tile_size: [256, 256],
                base_resolution: 1.0,
                origin: Default::default(),
            },
        );
        let err = format!("{:#}", Catalog::build(&config).err().expect("must fail"));
        assert!(err.contains("EPSG:99999"), "{err}");
    }
}
