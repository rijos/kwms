# Module layout

Single crate `kwms` (library + binary) with a `src/lib.rs` exposing the internal surface for integration tests and a thin `src/main.rs` driving the CLI. GDAL and PROJ are treated as load-bearing dependencies — no feature flags hide them, no traits exist to "abstract" them away.

## `src/` tree

```
src/
├── main.rs                # entry point — calls into cli
├── lib.rs                 # public surface for tests
├── cli.rs                 # clap definitions
├── subcommand/
│   ├── serve.rs
│   ├── cache_update.rs  # `kwms cache update`: fill durable tiers (auto level from source file size)
│   ├── config_check.rs
│   └── config_schema.rs
├── config/
│   ├── model.rs           # Config, LayerConfig, SourceConfig, CacheConfig, GridConfig, CrsRef, … (serde + schemars)
│   ├── load.rs            # read config.yaml / secrets.yaml, layer env + CLI overrides
│   ├── merge.rs           # deep-merge maps, replace lists
│   ├── validate.rs        # full validation pass (structural + CRS-aware)
│   └── schema.rs          # emit_json_schema()
├── ffi.rs                 # the only module allowed `unsafe`: raw GDAL calls behind safe fns (see unsafe-audit.md)
├── crs/
│   ├── mod.rs             # Crs newtype, AxisOrder, Bbox; free functions (is_geographic, native_axis_order, transform_bbox, validate, …)
│   └── gdal.rs            # GDAL/PROJ-backed implementations of the above (pub(crate))
├── tile/
│   ├── coord.rs           # TileCoord { z, x, y } and bbox conversions
│   ├── grid.rs            # TileGrid trait + WebMercator / GlobalGeodetic / Custom impls
│   └── pyramid.rs         # Pyramid type, level selection
├── source/
│   ├── mod.rs             # Source trait, WindowRequest, PixelBuffer, SourceMetadata, PixelValue
│   ├── registry.rs        # SourceRegistry: id -> Arc<dyn Source>
│   └── gdal/
│       ├── mod.rs         # GdalSource: impl Source
│       ├── dataset.rs     # gdal::Dataset wrapper + per-source open options (credentials, VSI tweaks)
│       ├── pool.rs        # bounded dataset-handle pool; checkouts via spawn_blocking
│       ├── warp.rs        # read_window: one GDALWarp path for every read
│       ├── raster.rs      # shared MEM/geotransform/interleave helpers + the single GDALWarp entry point
│       ├── sample.rs      # pixel sampling for GetFeatureInfo
│       ├── tile_index.rs  # GTI index probe: per-tile GSD, one view per GSD, auto overviews
│       ├── footprint.rs   # typical source-file size (for cache.max_level auto)
│       ├── runtime.rs     # process-wide GDAL settings: remote .tif only, serve timeouts, GTI threads, messages -> tracing
│       └── credentials.rs # per-bucket GDAL path-specific credentials
├── cache/
│   ├── mod.rs             # Cache facade
│   ├── key.rs             # TileKey { layer, grid_dir, z, x, y, format } + object path
│   ├── policy.rs          # per-layer / per-level mode resolution
│   ├── tier.rs            # CacheTier trait + Entry (tile + render time)
│   ├── memory.rs          # moka in-process tier
│   ├── store.rs           # opendal-backed tier (fs, azblob, …)
│   └── layered.rs         # tier stack (lookup, promotion, write-through) + single-flight
├── encode.rs              # GDAL-backed encoder for PNG / JPEG / WebP / GeoTIFF (see Output encoding chapter)
├── logging.rs             # JSON logs on the console; level from server.log_level (RUST_LOG wins)
├── redact.rs              # strip credentials (URL queries, sig=, AccountKey=) from anything logged
├── pipeline/
│   ├── render.rs          # TileKey -> cache lookup -> source -> encode -> populate
│   ├── cache_levels.rs    # cache.max_level: how deep each (layer, grid) is stored durably
│   ├── stage.rs           # kwms_stage_seconds timings (where request time goes)
│   ├── fingerprint.rs     # render-settings fingerprint stored with cached tiles (cache update invalidation)
│   └── wms_assemble.rs    # WMS GetMap: cover bbox with tiles, render, reassemble
├── protocol/
│   ├── wms/
│   │   ├── request.rs     # KVP parsing for GetCapabilities / GetMap / GetFeatureInfo (1.1.1 + 1.3.0)
│   │   ├── axis_order.rs  # version-specific axis-order rules; calls crs::native_axis_order
│   │   ├── capabilities.rs
│   │   └── exception.rs   # ServiceException response shapes per version
│   ├── wmts/
│   │   ├── request.rs     # KVP + RESTful path parsing
│   │   └── capabilities.rs
│   └── tms/
│       ├── request.rs     # path parsing + y-flip
│       └── capabilities.rs
└── server/
    ├── state.rs           # AppState { config, sources: SourceRegistry, cache: LayeredCache, ... }
    ├── routes/
    │   ├── wms.rs
    │   ├── wmts.rs
    │   ├── tms.rs
    │   ├── health.rs      # /healthz, /readyz
    │   └── metrics.rs     # /metrics (Prometheus)
    └── middleware/
        ├── tracing.rs     # per-request span (id, protocol, layer, tile coords)
        ├── metrics.rs     # request counters, latency histograms
        ├── cache_control.rs # Cache-Control from config; no-store for everything else
        └── request_id.rs  # random X-Request-Id; a client's is only logged
```

## `tests/` tree

Integration tests live in `tests/` at the repo root. Each `.rs` file is a separate test binary. Fixtures (sample configs, sample source rasters, pdok output files) live under `tests/fixtures/` and are checked into the repo.

```
tests/
├── capabilities.rs        # pdok-XML tests for WMS 1.1.1, WMS 1.3.0, WMTS 1.0.0, TMS 1.0.0 GetCapabilities
├── conformance.rs         # GetMap / GetTile / GetFeatureInfo response shapes, headers, axis order, ServiceException shapes
├── tile_render.rs         # pdok-tile diffs per layer/format/level (PNG byte-equality, JPEG/WebP perceptual)
├── cache_integration.rs   # hot+persistent tier behavior, TTL expiry, passthrough, level-rule overrides, single-flight
├── config_merge.rs        # YAML merge semantics, env-var/CLI overrides, JSON Schema export
├── common/
│   └── mod.rs             # shared test harness: spawn axum app in-process, request helpers, XML normalize+diff
└── fixtures/
    ├── config/
    │   ├── pdok_multigrid.yaml
    │   ├── single_grid_minimal.yaml
    │   ├── max_resolution_capped.yaml
    │   ├── flags_off.yaml
    │   └── custom_grid.yaml
    ├── sources/           # tiny COGs / VRTs used as deterministic source rasters
    │   ├── ortho_small.tif
    │   └── dem_small.tif
    └── pdok/
        ├── capabilities/  # one XML per (protocol-version × fixture)
        │   ├── wms_111_pdok_multigrid.xml
        │   ├── wms_130_pdok_multigrid.xml
        │   ├── wmts_pdok_multigrid.xml
        │   ├── tms_root_pdok_multigrid.xml
        │   └── …
        └── tiles/         # expected tile bytes per (layer × format × grid × z/x/y)
            ├── ortho2024/webmercator/0/0/0.jpeg
            └── …
```

See [Testing](./testing.md) for what each integration-test binary asserts and the pdok-file update workflow.

## Internal boundaries

Even within one crate, a few rules keep things honest:

- **Calls into the `gdal` crate are restricted to three modules**: `source/gdal/` (raster reads + warping), `crs/gdal.rs` (PROJ-backed CRS ops), and `encode.rs` (output writing via `CreateCopy` into `/vsimem/`). All other code uses the higher-level functions those modules expose. This isn't to hide GDAL — it's pervasive — but to keep PROJ initialization, driver registration, error mapping, and `/vsimem/` lifecycle in known locations.
- **`source::Source` is a trait** even though there's only one impl. It's a clean seam between "raster access" and "everything that consumes raster access" (cache, pipeline, protocol). Mocked in unit tests of the pipeline. Not a hedge against future non-GDAL backends — purely an internal contract.
- **`protocol` does no I/O.** Pure parsing in, pure response-shaping out. Handlers in `server/routes` are the only place protocol code meets the network and the pipeline.
- **`config` knows about `crs` for validation, but nothing else in `config` depends on the rest of the crate.** A small one-way arrow.

## Cargo features

None. GDAL/PROJ are required, all output formats are always built. If one format becomes a build pain in the future (webp, jpeg-turbo, …) we add a feature then. Premature feature-gating costs more than it saves.
