# Configuration

All runtime configuration resolves to a single in-memory `Config` object, assembled from layered sources at startup.

## Sources & precedence

From lowest to highest precedence (later overrides earlier):

1. **Built-in defaults** — compiled-in defaults for every optional field.
2. **`config.yaml`** — the main user-facing config: layer definitions, pyramids, cache rules, server bind address, etc. Layer order in this file is significant (it determines GetCapabilities order).
3. **`secrets.yaml`** — credentials only (per-source GDAL credentials, any future API keys). Separated so it can live outside source control / in a different secrets store.
4. **Environment variables** — prefix `KWMS_`, double-underscore for nesting (e.g. `KWMS_SERVER__BIND=0.0.0.0:8080`).
5. **CLI flags** — explicit overrides for the most common knobs (bind address, config path, log level). Not every field is exposed as a flag.

## Merge semantics

**Deep merge for maps, replace for lists.** Higher-precedence files merge into lower-precedence ones recursively at the map level; any list-valued field (notably `layers:`) is replaced wholesale, never concatenated. This preserves the "layer order in config.yaml = GetCapabilities order" guarantee — if you need to override a layer, you redeclare the whole `layers:` list at that precedence level.

## Hot reload

**Not supported in v1.** Config is loaded once at process start; changes require a restart. Revisit once in-flight request draining, pool reconfiguration, and cache lifecycle are sorted.

## Validation & schema

- Parsing via `serde` with `serde_yaml` (or `serde_yml`).
- A single validation pass after merging covers both structural and semantic constraints: TTLs are positive, level ranges don't overlap, layer refs resolve, formats are supported, source URIs are well-formed, EPSG codes resolve (via the `crs` module → GDAL/PROJ), advertised CRSes are compatible with the layer's declared grids, bbox values fall within the CRS's valid range. Errors include the offending file path and YAML location.
- `kwms config check` loads + merges + validates and exits — for CI use.
- `kwms config schema` emits a JSON Schema document for the merged config shape. Users point the VSCode YAML extension (or any schema-aware editor) at it for autocomplete and inline validation while editing `config.yaml` / `secrets.yaml`.

## File layout

The big simplification vs mapproxy: **no separate `sources:` / `caches:` / `grids:` sections cross-referenced by id**. Each layer is a self-contained block describing its own pyramid, sources, cache, and metadata. The only top-level shared section is `grids:`, and only for custom grids — well-known grids work by name with zero extra config.

Layer order in `config.yaml` is significant and reflected in GetCapabilities.

### `config.yaml` — full annotated example

```yaml
# ---- server-level settings ----
server:
  bind: "0.0.0.0:8080"
  max_request_pixels: [2500, 2500]   # WMS GetMap dimension cap (matches PDOK's default)
  log_level: info

# ---- cache backend (shared by all layers; per-layer policy lives on the layer) ----
cache:
  hot:
    max_bytes: "512MB"               # moka in-process tier
  persistent:
    backend: fs
    path: /var/cache/kwms
    # or, for cloud-backed caches:
    # backend: azure
    # container: tilecache
    # credentials: cache_blob        # references secrets.yaml

# ---- custom grids (omit if you only use well-known grids) ----
# Well-known grids shipped in v1 (usable as bare strings in a layer's grids: list):
#   webmercator           — OGC:1.0:GoogleMapsCompatible (EPSG:3857, 256px tiles)
#   global_geodetic       — WMTS GoogleCRS84Quad (EPSG:4326)
#   rd_new_quad           — NetherlandsRDNewQuad (EPSG:28992)
#   etrs89_laea_quad      — EuropeanETRS89_LAEAQuad (EPSG:3035)
#   utm_31n_quad, utm_32n_quad — power-of-2 UTM pyramids
grids:
  my_local:                          # custom grid; can be referenced by name from layers
    crs: EPSG:31370
    top_left: [-100000, 500000]      # (x, y) in the grid's CRS
    tile_size: [256, 256]
    base_resolution: 8192            # m/pixel at level 0; halved each level

# ---- layers (order is significant; advertised in this order) ----
layers:
  - name: ortho2024

    # Pyramid: which grids this layer is published in, and how deep.
    grids: [webmercator, rd_new_quad]
    max_resolution: native           # default: deepest level that reaches source's native res per grid
    # max_resolution: 0.5            # alternatively: cap at 0.5 m/pixel
    bbox: [3.17, 50.58, 7.49, 53.64] # optional WGS84 bbox; default = source bbox transformed to WGS84

    # Sources: list of GDAL-readable URIs. v1 uses the first; the list shape is for
    # forward-compat with multi-source composition.
    sources:
      - uri: "vsiaz://photos/ortho_2024.vrt"
        credentials: pdok_blob       # references secrets.yaml; omit if no credentials needed

    # Cache: mode + TTL + optional per-level overrides.
    cache:
      mode: cache                    # cache | passthrough (default: cache)
      ttl: 7d
      level_rules:
        - levels: 18-21
          mode: passthrough          # deep levels rendered fresh, not cached

    # Descriptions (advertised in GetCapabilities — WMS, WMTS, TMS).
    title: "Luchtfoto 2024 (8cm RGB)"
    abstract: "Landsdekkende dataset 8cm RGB luchtfoto's van 2024."
    keywords: [aerial, ortho, RGB, 2024]
    attribution: "Bron: PDOK / Kadaster"
    metadata_url: "https://www.nationaalgeoregister.nl/geonetwork/srv/api/records/..."

    # Rendering & output.
    formats: [jpeg, png]             # advertised + accepted output formats
    resampling: bilinear             # default: bilinear
    queryable: true                  # GetFeatureInfo support (default: true)
    opaque: true                     # WMS opaque hint; full-coverage rasters set this true (default: false)
    nodata: null                     # override source NoData; null = inherit from source
```

### `secrets.yaml` — credentials only

```yaml
credentials:
  pdok_blob:
    AZURE_STORAGE_ACCOUNT: "luchtfoto"
    AZURE_STORAGE_ACCESS_KEY: "..."
  aws_main:
    AWS_ACCESS_KEY_ID: "..."
    AWS_SECRET_ACCESS_KEY: "..."
  cache_blob:
    AZURE_STORAGE_ACCOUNT: "tilecache"
    AZURE_STORAGE_ACCESS_KEY: "..."
```

Each credentials block is a flat map of GDAL config options. At dataset open time, the options from the referenced block are applied as **dataset-scoped** GDAL open options (not process-wide env), so one server can host layers backed by different accounts without bleeding credentials.

## How the config drives the two protocols

A single `grids:` declaration on a layer drives both WMS- and WMTS-advertised endpoints:

| Layer config | WMS 1.1.1 / 1.3.0 GetCapabilities | WMTS 1.0.0 GetCapabilities |
|---|---|---|
| `grids: [webmercator, rd_new_quad]` | Layer advertises `EPSG:3857` and `EPSG:28992` as supported CRSes. | Layer links to `GoogleMapsCompatible` and `NetherlandsRDNewQuad` `TileMatrixSetLink`s. |
| `max_resolution: native` | (No direct effect — WMS clients request any size.) | Each TileMatrixSet's `TileMatrix` count is derived per grid to reach native source res. PDOK-style 22 levels for WebMercator, 20 for RDNew — automatically. |
| `bbox: [...]` | Drives `EX_GeographicBoundingBox` and per-CRS `BoundingBox` (transformed via GDAL/PROJ). | Drives the layer's `WGS84BoundingBox`. |
| `queryable: true` | `queryable="1"` on the Layer element; GetFeatureInfo routed. | (WMTS has no equivalent flag.) |
| `opaque: true` | `opaque="1"` on the Layer element. | (No equivalent.) |
| `formats: [jpeg, png]` | `GetMap`-supported formats; per-request `FORMAT=image/jpeg`. | Per-layer `Format` elements; per-tile `.jpeg` / `.png` extension. |

## Why this is shorter than mapproxy

- No `sources:` → `caches:` → `layers:` reference chain. A layer holds its own URIs.
- No `grids:` section for the common case. Bare strings reference well-known grids; the entire pyramid math is derived from a name + the source's native resolution.
- No explicit `resolutions: [...]` list. A WMTS-style power-of-2 pyramid is fully specified by `{grid identity, depth}`, and depth defaults to "go to native."
- No DPI/scale-denominator config. We render to pixel buffers; clients that care about scale denominators read them out of the auto-generated WMTS TileMatrixSet. (Printing/DPI is out of scope.)
- Credentials are referenced by name once, not duplicated per source.
