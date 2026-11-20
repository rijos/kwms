//! Configuration data types — what `config.yaml` and `secrets.yaml`
//! deserialize into.

use std::collections::HashMap;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::crs::{Bbox, Crs};
use crate::encode::OutputFormat;

/// Root config object — the result of merging defaults, `config.yaml`,
/// `secrets.yaml`, env vars, and CLI overrides.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub cache: GlobalCacheConfig,
    /// Custom (non-well-known) grid definitions, keyed by name.
    #[serde(default)]
    pub grids: HashMap<String, GridConfig>,
    #[serde(default)]
    pub layers: Vec<LayerConfig>,
    /// Default `source_mask` for every layer: a polygon vector dataset
    /// (shapefile, GeoPackage, …); only areas inside its polygons are shown,
    /// the rest is no data (transparent in PNG, white in JPEG). A layer's
    /// own `source_mask` overrides it.
    #[serde(default)]
    pub source_mask: Option<String>,
    /// Encoder settings per output format (quality, compression, …). Which
    /// formats a layer offers is its `formats:` list; a layer can override
    /// a format's settings with its own `output_formats:`.
    #[serde(default)]
    pub output_formats: OutputFormatsConfig,
    /// What capabilities documents say about the service: title, abstract,
    /// keywords, fees, access constraints, provider and contact.
    #[serde(default)]
    pub service: ServiceMetadata,
    /// WMTS publication: endpoint path and identifier style.
    #[serde(default)]
    pub wmts: WmtsConfig,
}

// ---- service metadata (capabilities) ----

/// Service-level metadata advertised in GetCapabilities. Unset fields are
/// left out of the documents (titles fall back to "kwms WMTS" etc.).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceMetadata {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default, rename = "abstract")]
    pub abstract_: Option<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub fees: Option<String>,
    #[serde(default)]
    pub access_constraints: Option<String>,
    #[serde(default)]
    pub provider: ServiceProvider,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceProvider {
    #[serde(default)]
    pub name: Option<String>,
    /// Provider web site (`ows:ProviderSite xlink:href`).
    #[serde(default)]
    pub site: Option<String>,
    #[serde(default)]
    pub contact: Option<ServiceContact>,
}

/// `ows:ServiceContact`. Fields left unset are written empty, as OWS
/// clients expect the structure to be complete.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceContact {
    #[serde(default)]
    pub individual_name: Option<String>,
    #[serde(default)]
    pub position_name: Option<String>,
    #[serde(default)]
    pub voice: Option<String>,
    #[serde(default)]
    pub facsimile: Option<String>,
    #[serde(default)]
    pub delivery_point: Option<String>,
    #[serde(default)]
    pub city: Option<String>,
    #[serde(default)]
    pub administrative_area: Option<String>,
    #[serde(default)]
    pub postal_code: Option<String>,
    #[serde(default)]
    pub country: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub hours_of_service: Option<String>,
    #[serde(default)]
    pub contact_instructions: Option<String>,
}

// ---- WMTS publication ----

/// How the WMTS endpoint is published. The defaults are the OGC well-known
/// names (`NetherlandsRDNewQuad`, TileMatrix `0`…, URN CRS ids) at `/wmts`;
/// PDOK-style publication is `tile_matrix_digits: 2`, `crs_identifier:
/// epsg` and `EPSG:…` TileMatrixSet ids.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WmtsConfig {
    /// URL path of the endpoint: KVP requests at it, RESTful tiles
    /// (`{path}/{layer}/{TileMatrixSet}/{TileMatrix}/{col}/{row}.{ext}`) and
    /// `{path}/1.0.0/WMTSCapabilities.xml` below it. E.g.
    /// `/hwh/luchtfotorgb/wmts/v1_0` to mirror PDOK.
    #[serde(default = "default_wmts_path")]
    pub path: String,
    /// Zero-pad TileMatrix identifiers to this many digits (PDOK: 2, so
    /// `00`…`19`). 0 = no padding. Requests accept both forms.
    #[serde(default)]
    pub tile_matrix_digits: u8,
    /// How `ows:SupportedCRS` is written.
    #[serde(default)]
    pub crs_identifier: CrsIdentifierStyle,
    /// TileMatrixSet identifiers, in advertised order. A grid may be
    /// published under several (PDOK: the web mercator grid as both
    /// `EPSG:3857` and `OGC:1.0:GoogleMapsCompatible`); a grid not listed
    /// keeps its default name.
    #[serde(default)]
    pub tile_matrix_sets: Vec<TileMatrixSetName>,
}

impl Default for WmtsConfig {
    fn default() -> Self {
        Self {
            path: default_wmts_path(),
            tile_matrix_digits: 0,
            crs_identifier: CrsIdentifierStyle::default(),
            tile_matrix_sets: Vec::new(),
        }
    }
}

fn default_wmts_path() -> String {
    "/wmts".into()
}

impl WmtsConfig {
    /// The TileMatrixSet identifiers `grid` is published under: its
    /// configured names in order, else its default name.
    pub fn tile_matrix_set_ids(&self, grid: &str) -> Vec<String> {
        let named: Vec<String> = self
            .tile_matrix_sets
            .iter()
            .filter(|t| t.grid == grid)
            .map(|t| t.identifier.clone())
            .collect();
        if named.is_empty() {
            vec![crate::tile::grid::tile_matrix_set_name(grid).to_string()]
        } else {
            named
        }
    }

    /// The identifier of TileMatrix `level`.
    pub fn tile_matrix_id(&self, level: u32) -> String {
        format!("{level:0width$}", width = usize::from(self.tile_matrix_digits))
    }
}

/// One published TileMatrixSet name.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TileMatrixSetName {
    /// Grid name (`rd_new_quad`, `webmercator`, or a custom grid's key).
    pub grid: String,
    /// TileMatrixSet identifier, e.g. `EPSG:28992`.
    pub identifier: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CrsIdentifierStyle {
    /// `urn:ogc:def:crs:EPSG::28992`
    #[default]
    Urn,
    /// `EPSG:28992` (PDOK)
    Epsg,
}

// ---- output formats ----

/// Encoder settings for every format. Each defaults to kwms's web-tile
/// tuning, so only what you change needs to be written.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct OutputFormatsConfig {
    #[serde(default)]
    pub jpeg: JpegSettings,
    #[serde(default)]
    pub png: PngSettings,
    #[serde(default)]
    pub webp: WebpSettings,
    #[serde(default)]
    pub geotiff: GeoTiffSettings,
}

/// A layer's overrides: a format listed here replaces the global settings
/// for that format (unset fields take kwms's defaults, not the global ones).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LayerOutputFormats {
    #[serde(default)]
    pub jpeg: Option<JpegSettings>,
    #[serde(default)]
    pub png: Option<PngSettings>,
    #[serde(default)]
    pub webp: Option<WebpSettings>,
    #[serde(default)]
    pub geotiff: Option<GeoTiffSettings>,
}

impl OutputFormatsConfig {
    /// These settings with a layer's overrides applied.
    pub fn with_overrides(&self, layer: &LayerOutputFormats) -> Self {
        Self {
            jpeg: layer.jpeg.clone().unwrap_or_else(|| self.jpeg.clone()),
            png: layer.png.clone().unwrap_or_else(|| self.png.clone()),
            webp: layer.webp.clone().unwrap_or_else(|| self.webp.clone()),
            geotiff: layer.geotiff.clone().unwrap_or_else(|| self.geotiff.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JpegSettings {
    /// 1–100.
    #[serde(default = "default_jpeg_quality")]
    pub quality: u8,
    #[serde(default)]
    pub progressive: bool,
}

impl Default for JpegSettings {
    fn default() -> Self {
        Self {
            quality: default_jpeg_quality(),
            progressive: false,
        }
    }
}

fn default_jpeg_quality() -> u8 {
    85
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PngSettings {
    /// zlib level 1 (fastest) – 9 (smallest).
    #[serde(default = "default_png_compression")]
    pub compression: u8,
}

impl Default for PngSettings {
    fn default() -> Self {
        Self {
            compression: default_png_compression(),
        }
    }
}

fn default_png_compression() -> u8 {
    6
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebpSettings {
    /// 1–100 (ignored when lossless).
    #[serde(default = "default_webp_quality")]
    pub quality: u8,
    #[serde(default)]
    pub lossless: bool,
}

impl Default for WebpSettings {
    fn default() -> Self {
        Self {
            quality: default_webp_quality(),
            lossless: false,
        }
    }
}

fn default_webp_quality() -> u8 {
    80
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GeoTiffSettings {
    #[serde(default)]
    pub compress: GeoTiffCompression,
    /// TIFF predictor: 1 none, 2 horizontal, 3 floating point.
    #[serde(default = "default_geotiff_predictor")]
    pub predictor: u8,
    #[serde(default = "default_true")]
    pub tiled: bool,
}

impl Default for GeoTiffSettings {
    fn default() -> Self {
        Self {
            compress: GeoTiffCompression::default(),
            predictor: default_geotiff_predictor(),
            tiled: true,
        }
    }
}

fn default_geotiff_predictor() -> u8 {
    2
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum GeoTiffCompression {
    None,
    #[default]
    Deflate,
    Lzw,
    Zstd,
}

// ---- server ----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Absolute URL clients reach this server at (e.g.
    /// `http://localhost:8080` or `https://tiles.example.org`). Used to
    /// build the absolute URLs in capabilities documents, which clients
    /// such as QGIS follow. Not set: server-relative URLs (`/wms`).
    #[serde(default)]
    pub public_url: Option<String>,
    #[serde(default = "default_max_request_pixels")]
    pub max_request_pixels: [u32; 2],
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// Separate listener for `/metrics`, kept off the public port so it can
    /// be firewalled independently. `null` disables the metrics endpoint.
    #[serde(default = "default_metrics_bind")]
    pub metrics_bind: Option<String>,
    /// Wall-clock budget for one WMS/WMTS/TMS request; exceeded → 503.
    #[serde(default = "default_request_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub request_timeout: Duration,
    /// Protocol requests in flight at once; excess requests are shed with 429.
    #[serde(default = "default_max_concurrent_requests")]
    pub max_concurrent_requests: usize,
    /// CPU-bound render jobs (encode, WMS compositing) running at once.
    /// Defaults to the number of available CPUs.
    #[serde(default)]
    pub render_concurrency: Option<usize>,
    /// How long a request may wait for a render slot before getting 429.
    #[serde(default = "default_render_queue_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub render_queue_timeout: Duration,
    /// Limit on one HTTP request to a source's object store while serving
    /// (connect + transfer). A stalled connection fails after this instead
    /// of holding a source worker forever. Must be below `request_timeout`.
    /// `kwms cache update` has no limit.
    #[serde(default = "default_source_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub source_timeout: Duration,
    /// Threads one tile-index read may use while serving (GDAL's GTI reads
    /// the files under a request in parallel). `kwms cache update` uses all
    /// CPUs.
    #[serde(default = "default_tile_index_threads")]
    pub tile_index_threads: usize,
    /// Time a client gets to send a request's headers, and the longest an
    /// idle keep-alive connection is kept open. Defends against slow-header
    /// ("slowloris") clients.
    #[serde(default = "default_header_read_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub header_read_timeout: Duration,
    /// Open client connections at once; further connections wait in the
    /// listen backlog until one closes.
    #[serde(default = "default_max_connections")]
    pub max_connections: usize,
    /// `Cache-Control` response headers (see [`CacheControlConfig`]).
    #[serde(default)]
    pub cache_control: CacheControlConfig,
}

/// `Cache-Control` values kwms sends. Error responses (including WMS
/// exceptions, which are HTTP 200) always get `no-store`, whatever is set
/// here, so a shared cache never keeps a transient failure. An empty string
/// sends no header.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CacheControlConfig {
    /// Tiles (WMTS, TMS) and WMS GetMap images. A layer can override it.
    #[serde(default = "default_cache_control_images")]
    pub images: String,
    /// Capabilities and other metadata documents.
    #[serde(default = "default_cache_control_capabilities")]
    pub capabilities: String,
}

impl Default for CacheControlConfig {
    fn default() -> Self {
        Self {
            images: default_cache_control_images(),
            capabilities: default_cache_control_capabilities(),
        }
    }
}

fn default_cache_control_images() -> String {
    "public, max-age=86400".into()
}
fn default_cache_control_capabilities() -> String {
    "public, max-age=3600".into()
}
fn default_source_timeout() -> Duration {
    Duration::from_secs(20)
}
fn default_tile_index_threads() -> usize {
    4
}
fn default_header_read_timeout() -> Duration {
    Duration::from_secs(10)
}
fn default_max_connections() -> usize {
    4096
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            public_url: None,
            max_request_pixels: default_max_request_pixels(),
            log_level: default_log_level(),
            metrics_bind: default_metrics_bind(),
            request_timeout: default_request_timeout(),
            max_concurrent_requests: default_max_concurrent_requests(),
            render_concurrency: None,
            render_queue_timeout: default_render_queue_timeout(),
            source_timeout: default_source_timeout(),
            tile_index_threads: default_tile_index_threads(),
            header_read_timeout: default_header_read_timeout(),
            max_connections: default_max_connections(),
            cache_control: CacheControlConfig::default(),
        }
    }
}

impl ServerConfig {
    /// Effective render concurrency (`render_concurrency` or the CPU count).
    pub fn effective_render_concurrency(&self) -> usize {
        self.render_concurrency
            .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4))
    }
}

fn default_bind() -> String {
    "0.0.0.0:8080".into()
}
fn default_metrics_bind() -> Option<String> {
    // Loopback: scraped by a sidecar (Istio metrics merging) or a local
    // agent, never exposed by accident.
    Some("127.0.0.1:9091".into())
}
fn default_request_timeout() -> Duration {
    Duration::from_secs(30)
}
fn default_max_concurrent_requests() -> usize {
    512
}
fn default_render_queue_timeout() -> Duration {
    Duration::from_secs(5)
}
fn default_max_request_pixels() -> [u32; 2] {
    [2500, 2500]
}
fn default_log_level() -> String {
    "info".into()
}

// ---- cache (global) ----

/// The tile cache: an ordered stack of tiers, checked top to bottom.
///
/// ```yaml
/// cache:
///   tiers:
///     - type: memory            # in-process, lost on restart
///       max_bytes: 512MB
///     - type: store             # any opendal backend
///       backend: fs
///       options: { root: /var/cache/kwms }
/// ```
///
/// A hit in a lower tier is copied into the tiers above it; a rendered tile
/// is written to every tier. Default: the two tiers above. An empty list
/// disables caching. See `docs/src/caching.md`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobalCacheConfig {
    #[serde(default = "default_tiers")]
    pub tiers: Vec<TierConfig>,
}

impl Default for GlobalCacheConfig {
    fn default() -> Self {
        Self { tiers: default_tiers() }
    }
}

fn default_tiers() -> Vec<TierConfig> {
    vec![
        TierConfig::Memory(MemoryTierConfig::default()),
        TierConfig::Store(StoreTierConfig::default()),
    ]
}

/// One cache tier.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum TierConfig {
    /// In-process memory (moka), bounded by total bytes.
    Memory(MemoryTierConfig),
    /// An opendal storage backend: `fs`, `azblob`, …
    Store(StoreTierConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MemoryTierConfig {
    /// Human-friendly byte size, e.g. `"512MB"`. Parsed by `bytesize`.
    #[serde(default = "default_memory_max")]
    pub max_bytes: String,
}

impl Default for MemoryTierConfig {
    fn default() -> Self {
        Self {
            max_bytes: default_memory_max(),
        }
    }
}

fn default_memory_max() -> String {
    "512MB".into()
}

/// A storage-backend tier — delegated to opendal.
///
/// `backend` is an opendal service name compiled into kwms (`fs`,
/// `azblob`, `memory`). `options` is passed verbatim to
/// [`opendal::Operator::via_iter`]; per-backend keys:
/// <https://docs.rs/opendal/0.59/opendal/services/index.html>.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct StoreTierConfig {
    pub backend: String,
    #[serde(default)]
    pub options: HashMap<String, String>,
    /// Optional reference into `secrets.yaml::credentials.<name>`, merged
    /// into `options` (an inline option wins) so secrets stay out of
    /// `config.yaml`.
    #[serde(default)]
    pub credentials: Option<String>,
}

impl Default for StoreTierConfig {
    fn default() -> Self {
        let mut options = HashMap::new();
        options.insert("root".into(), "/var/cache/kwms".into());
        Self {
            backend: "fs".into(),
            options,
            credentials: None,
        }
    }
}

// ---- grids ----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GridConfig {
    pub crs: Crs,
    pub top_left: [f64; 2],
    #[serde(default = "default_tile_size")]
    pub tile_size: [u32; 2],
    pub base_resolution: f64,
    #[serde(default)]
    pub origin: GridOrigin,
    /// Bounded tile matrix: `[minx, miny, maxx, maxy]` in the grid's CRS
    /// (x = easting / longitude). Each level then has
    /// `ceil(extent / tile size)` columns and rows instead of 2^level —
    /// e.g. PDOK's EPSG:4258 grid is 1x1 at level 0 and 524288x358367 at
    /// level 19. Its top-left must be `top_left`. Unset: power-of-two
    /// ladder, one tile at level 0.
    #[serde(default)]
    pub extent: Option<[f64; 4]>,
    /// Number of zoom levels (default 24; PDOK's custom grids have 20).
    #[serde(default)]
    pub levels: Option<u32>,
}

fn default_tile_size() -> [u32; 2] {
    [256, 256]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GridOrigin {
    #[default]
    TopLeft,
    BottomLeft,
}

// ---- layer ----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LayerConfig {
    pub name: String,

    pub grids: Vec<String>,
    #[serde(default)]
    pub max_resolution: MaxResolution,
    /// Extent advertised in capabilities, `[minx, miny, maxx, maxy]` in
    /// `bbox_crs` (default WGS84 lon/lat). Optional: by default the source's
    /// own extent in its native CRS (a tile index's footprints, a raster's
    /// extent). BoundingBoxes in that CRS are exact; the others (and the
    /// WGS84 boxes) are reprojected from it.
    #[serde(default)]
    pub bbox: Option<Bbox>,
    /// CRS of `bbox` (default `EPSG:4326`, lon/lat order).
    #[serde(default)]
    pub bbox_crs: Option<Crs>,

    pub sources: Vec<SourceConfig>,

    #[serde(default)]
    pub cache: LayerCacheConfig,

    pub title: String,
    #[serde(default, rename = "abstract")]
    pub abstract_: String,
    #[serde(default)]
    pub keywords: Vec<String>,
    #[serde(default)]
    pub attribution: Option<String>,
    #[serde(default)]
    pub metadata_url: Option<String>,

    pub formats: Vec<OutputFormat>,
    #[serde(default = "default_resampling")]
    pub resampling: String,
    #[serde(default = "default_true")]
    pub queryable: bool,
    #[serde(default)]
    pub opaque: bool,
    #[serde(default)]
    pub nodata: Option<f64>,
    /// Polygon vector dataset limiting where this layer shows data;
    /// overrides the top-level `source_mask`. `none` disables a global one
    /// for this layer.
    #[serde(default)]
    pub source_mask: Option<String>,
    /// Per-format encoder settings for this layer, replacing the top-level
    /// `output_formats` for the formats listed.
    #[serde(default)]
    pub output_formats: LayerOutputFormats,
    /// `Cache-Control` for this layer's images, replacing
    /// `server.cache_control.images` (e.g. `no-cache` for a layer that
    /// changes often). Empty: no header.
    #[serde(default)]
    pub cache_control: Option<String>,
}

impl LayerConfig {
    /// The mask that applies to this layer (its own, else the global one),
    /// `None` when there is none or it is `none`.
    pub fn effective_source_mask<'a>(&'a self, global: Option<&'a str>) -> Option<&'a str> {
        match self.source_mask.as_deref() {
            Some(m) if m.eq_ignore_ascii_case("none") => None,
            Some(m) => Some(m),
            None => global,
        }
    }
}

fn default_resampling() -> String {
    "bilinear".into()
}
fn default_true() -> bool {
    true
}

/// `max_resolution: native` (default) or `max_resolution: 0.5` (meters per pixel).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum MaxResolution {
    /// Numeric override (m/pixel).
    MetersPerPixel(f64),
    /// `"native"` keyword.
    Mode(MaxResolutionMode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MaxResolutionMode {
    Native,
}

impl Default for MaxResolution {
    fn default() -> Self {
        MaxResolution::Mode(MaxResolutionMode::Native)
    }
}

// ---- source (per-layer) ----

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    /// GDAL dataset name, passed verbatim (a path, `/vsiaz/…`, `GTI:…`, …).
    /// Relative paths resolve against kwms's working directory.
    pub uri: String,
    /// GDAL open options as `KEY=VALUE` strings (what `gdalinfo -oo` takes),
    /// e.g. `OVERVIEW_0_FACTOR=2` for a `GTI:` tile index.
    #[serde(default)]
    pub open_options: Vec<String>,
    /// Name of a block in `secrets.yaml::credentials`.
    #[serde(default)]
    pub credentials: Option<String>,
    /// The `/vsiXXX/<bucket>/…` path prefix the credentials apply to (GDAL
    /// path-specific options). Defaults to `/vsiXXX/<bucket>/` taken from
    /// `uri`; required when `uri` is not itself an object-store path (e.g. a
    /// local tile index or VRT whose tiles live in a bucket).
    #[serde(default)]
    pub credentials_prefix: Option<String>,
}

// ---- layer cache ----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LayerCacheConfig {
    #[serde(default)]
    pub mode: CacheMode,
    /// Deepest level stored in the durable tiers (disk, Azure, …), per grid.
    /// `auto` (default): the deepest level whose tiles are larger than one
    /// source file — beyond it, rendering from the source is about as fast as
    /// it gets, so storing tiles buys little. A number caps it explicitly;
    /// `all` stores every level. Deeper tiles are kept in the in-process
    /// tiers only (`memory` mode). `kwms cache update` fills up to the same
    /// level.
    #[serde(default)]
    pub max_level: CacheMaxLevel,
    #[serde(default = "default_ttl", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub ttl: Duration,
    #[serde(default)]
    pub level_rules: Vec<LevelRule>,
}

impl Default for LayerCacheConfig {
    fn default() -> Self {
        Self {
            mode: CacheMode::Cache,
            max_level: CacheMaxLevel::default(),
            ttl: default_ttl(),
            level_rules: Vec::new(),
        }
    }
}

fn default_ttl() -> Duration {
    Duration::from_secs(7 * 24 * 60 * 60)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CacheMode {
    /// Every tier.
    #[default]
    Cache,
    /// In-process tiers only (not stored on disk / in object storage).
    Memory,
    /// No caching: rendered on every request.
    Passthrough,
}

/// `cache.max_level`: `auto`, `all`, or a level number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(untagged)]
pub enum CacheMaxLevel {
    Level(u32),
    Mode(CacheMaxLevelMode),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum CacheMaxLevelMode {
    /// Levels whose tiles are larger than one source file.
    Auto,
    /// Every level.
    All,
}

impl Default for CacheMaxLevel {
    fn default() -> Self {
        Self::Mode(CacheMaxLevelMode::Auto)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LevelRule {
    /// Inclusive level range, e.g. `"18-21"`. Parsed by the validator.
    pub levels: String,
    pub mode: CacheMode,
}

// ---- secrets.yaml ----

/// Credentials block — a flat map of GDAL config options, applied at dataset
/// open time as scoped open options (not process-wide env).
#[derive(Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct Credentials(pub HashMap<String, String>);

/// Prints keys only — values are secrets and must never reach logs.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut keys: Vec<&String> = self.0.keys().collect();
        keys.sort();
        f.debug_map()
            .entries(keys.into_iter().map(|k| (k, "<redacted>")))
            .finish()
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Secrets {
    #[serde(default)]
    pub credentials: HashMap<String, Credentials>,
}

#[cfg(test)]
mod tier_config_tests {
    use super::*;

    #[test]
    fn tiers_parse_in_order_and_reject_typos() {
        let yaml = "tiers:\n  - type: memory\n    max_bytes: 64MB\n  - type: store\n    backend: fs\n    options: {root: /x}\n  - type: store\n    backend: azblob\n    credentials: c\n";
        let cfg: GlobalCacheConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(
            matches!(&cfg.tiers[..], [TierConfig::Memory(m), TierConfig::Store(a), TierConfig::Store(b)]
            if m.max_bytes == "64MB" && a.backend == "fs" && b.credentials.as_deref() == Some("c"))
        );

        for bad in [
            "tiers: [{type: memory, max_byte: 1MB}]",
            "tiers: [{type: disk}]",
            "hot: {max_bytes: 1MB}",
        ] {
            assert!(serde_yaml::from_str::<GlobalCacheConfig>(bad).is_err(), "{bad}");
        }
        assert_eq!(GlobalCacheConfig::default().tiers.len(), 2);
    }
}
