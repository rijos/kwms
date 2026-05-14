//! Configuration data types — what `config.yaml` and `secrets.yaml`
//! deserialize into.

use std::collections::HashMap;
use std::path::PathBuf;
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
}

// ---- server ----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    #[serde(default = "default_bind")]
    pub bind: String,
    #[serde(default = "default_max_request_pixels")]
    pub max_request_pixels: [u32; 2],
    #[serde(default = "default_log_level")]
    pub log_level: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: default_bind(),
            max_request_pixels: default_max_request_pixels(),
            log_level: default_log_level(),
        }
    }
}

fn default_bind() -> String {
    "0.0.0.0:8080".into()
}
fn default_max_request_pixels() -> [u32; 2] {
    [2500, 2500]
}
fn default_log_level() -> String {
    "info".into()
}

// ---- cache backend (global) ----

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobalCacheConfig {
    #[serde(default)]
    pub hot: HotCacheConfig,
    #[serde(default)]
    pub persistent: PersistentCacheConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HotCacheConfig {
    /// Human-friendly byte size, e.g. `"512MB"`. Parsed by `bytesize`.
    #[serde(default = "default_hot_max")]
    pub max_bytes: String,
}

impl Default for HotCacheConfig {
    fn default() -> Self {
        Self { max_bytes: default_hot_max() }
    }
}

fn default_hot_max() -> String {
    "512MB".into()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "backend", rename_all = "lowercase")]
pub enum PersistentCacheConfig {
    Fs {
        path: PathBuf,
    },
    Azure {
        container: String,
        #[serde(default)]
        credentials: Option<String>,
    },
    S3 {
        bucket: String,
        #[serde(default)]
        credentials: Option<String>,
    },
    Redis {
        url: String,
    },
    Memcached {
        url: String,
    },
}

impl Default for PersistentCacheConfig {
    fn default() -> Self {
        Self::Fs { path: PathBuf::from("/var/cache/kwms") }
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
    #[serde(default)]
    pub bbox: Option<Bbox>,

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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    pub uri: String,
    #[serde(default)]
    pub credentials: Option<String>,
}

// ---- layer cache ----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LayerCacheConfig {
    #[serde(default)]
    pub mode: CacheMode,
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
    #[default]
    Cache,
    Passthrough,
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
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct Credentials(pub HashMap<String, String>);

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Secrets {
    #[serde(default)]
    pub credentials: HashMap<String, Credentials>,
}
