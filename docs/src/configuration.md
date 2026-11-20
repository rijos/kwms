# Configuration

All runtime configuration resolves to a single in-memory `Config` object, assembled from layered sources at startup.

## Sources & precedence

From lowest to highest precedence (later overrides earlier):

1. **Built-in defaults** — compiled-in defaults for every optional field.
2. **`config.yaml`** — the main user-facing config: layer definitions, pyramids, cache rules, server bind address, etc. Layer order in this file is significant (it determines GetCapabilities order).
3. **`secrets.yaml`** — credentials only (per-source GDAL credentials, cache store credentials, any future API keys). Separated so it can live outside source control / in a different secrets store.
4. **Environment variables** — prefix `KWMS_`, double-underscore for nesting (e.g. `KWMS_SERVER__BIND=0.0.0.0:8080`). Only variables whose first key is a top-level section (`server`, `cache`, `layers`, …) count; others are ignored and logged by name. So the `KWMS_PORT`, `KWMS_SERVICE_HOST`, … that Kubernetes injects for a Service named `kwms` don't stop kwms from starting. Names and values must be UTF-8.

Config errors name the field and the problem but never quote the offending value: secrets.yaml, environment overrides and inline store options carry credentials.
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
  public_url: "https://tiles.example.org"  # base of every URL in capabilities (QGIS & co. follow
                                         # them); absolute http(s), no query. Default: none =
                                         # server-relative links (/wms, /wmts/…)
  max_request_pixels: [2500, 2500]   # WMS GetMap dimension cap (matches PDOK's default)
  log_level: info                    # or RUST_LOG-style directives, e.g. "info,gdal=warn,kwms::server=debug".
                                     # RUST_LOG, when set, wins.
  metrics_bind: "127.0.0.1:9091"     # /metrics listener (default: loopback, for a sidecar/agent to scrape).
                                     # null = disabled
  request_timeout: 30s               # per WMS/WMTS/TMS request; exceeded → 503
  source_timeout: 20s                # one HTTP request to a source's object store while serving; a stalled
                                     # connection fails after this. Below request_timeout. (cache update: none)
  tile_index_threads: 4              # threads one tile-index (GTI) read may use while serving
                                     # (cache update: all CPUs)
  header_read_timeout: 10s           # time to send request headers; also the idle keep-alive limit
  max_connections: 4096              # open client connections; more wait in the listen backlog
  cache_control:                     # Cache-Control headers; errors always get no-store. "" = no header
    images: "public, max-age=86400"  # tiles and GetMap images (a layer's `cache_control` replaces it)
    capabilities: "public, max-age=3600"
  max_concurrent_requests: 512       # in-flight protocol requests; excess shed with 429
  # render_concurrency: 8            # parallel encode/composite jobs; default = CPU count
  render_queue_timeout: 5s           # max wait for a render slot before 429

# ---- source mask (optional) ----
# Polygon vector dataset: only areas inside its polygons are shown; the rest is
# no data (transparent in PNG, white in JPEG), like areas outside the source.
# Applies to every layer; a layer's own `source_mask` overrides it (`none` = off).
source_mask: data/source_mask.shp

# ---- cache (shared by all layers; per-layer policy lives on the layer) ----
# An ordered stack of tiers, checked top to bottom. A hit in a lower tier is
# copied into the tiers above it (keeping its render time, so the layer's TTL
# still counts from the render); a rendered tile is written to every tier.
# Default when `cache:` is omitted: the memory + fs pair below. `tiers: []`
# disables caching. See caching.md for the layout and failure behaviour.
cache:
  tiers:
    - type: memory                   # in-process (moka), lost on restart
      max_bytes: 512MB
    - type: store                    # an opendal storage backend
      # `backend` is an opendal Scheme name; `options` is handed verbatim to
      # Operator::via_iter. Per-backend keys:
      # https://docs.rs/opendal/0.59/opendal/services/
      # Compiled in: `fs`, `azblob`, `memory` (in-process; not durable).
      # Others (s3, gcs, redis, …) need their `services-*` opendal feature
      # in Cargo.toml.
      backend: fs
      options:
        root: /var/cache/kwms
    # A shared Azure tier below the local disk (memory -> fs -> azblob). The
    # container must already exist; the credentials need read, write and
    # delete.
    # - type: store
    #   backend: azblob
    #   options:
    #     endpoint: https://acct.blob.core.windows.net
    #     container: tilecache
    #     root: /                    # optional prefix inside the container
    #   credentials: cache_blob      # merged into `options` from secrets.yaml

# ---- custom grids (omit if you only use well-known grids) ----
# Well-known grids shipped in v1 (usable as bare strings in a layer's grids: list):
#   webmercator           — OGC:1.0:GoogleMapsCompatible (EPSG:3857, 256px tiles)
#   global_geodetic       — WorldCRS84Quad (EPSG:4326, 2x1 tiles at level 0; TMS global-geodetic).
#                           WMTS requests for the old name GoogleCRS84Quad are still accepted.
#   rd_new_quad           — NetherlandsRDNewQuad (EPSG:28992)
#   etrs89_laea_quad      — EuropeanETRS89_LAEAQuad (EPSG:3035)
# The list lives in `tile::grid::WELL_KNOWN_GRIDS`; custom grids may not reuse these names.
grids:
  my_local:                          # custom grid; can be referenced by name from layers
    crs: EPSG:31370
    top_left: [-100000, 500000]      # (x, y) in the grid's CRS
    tile_size: [256, 256]
    base_resolution: 8192            # m/pixel at level 0; halved each level
    # levels: 20                     # zoom levels (default 24)
    # extent: [minx, miny, maxx, maxy]  # bounded matrix: ceil(extent / tile) tiles per level
    #                                # instead of 2^level; its top-left must be top_left

# ---- service metadata (GetCapabilities) ----
service:
  title: Landelijke Voorziening Beeldmateriaal
  abstract: Een jaarlijks te vernieuwen dataset van luchtopnamen van Nederland.
  keywords: [luchtfoto, orthofoto]
  fees: none
  access_constraints: none
  provider:
    name: PDOK
    site: https://www.pdok.nl
    contact:                         # any of: individual_name, position_name, voice, facsimile,
      individual_name: KlantContactCenter PDOK   # delivery_point, city, administrative_area,
      email: BeheerPDOK@kadaster.nl  # postal_code, country, email, hours_of_service, contact_instructions

# ---- WMTS publication ----
wmts:
  path: /wmts                        # KVP here; tiles at {path}/{layer}/{TileMatrixSet}/{TileMatrix}/{col}/{row}.{ext};
                                     # {path}/1.0.0/WMTSCapabilities.xml
  tile_matrix_digits: 0              # zero-pad TileMatrix ids (2 -> 00, 01, …); requests accept both
  crs_identifier: urn                # SupportedCRS: urn (urn:ogc:def:crs:EPSG::28992) | epsg (EPSG:28992)
  tile_matrix_sets:                  # identifiers per grid, advertised in this order; unlisted grids keep
    - { grid: rd_new_quad, identifier: "EPSG:28992" }   # their default (NetherlandsRDNewQuad, …)
    - { grid: webmercator, identifier: "EPSG:3857" }
    - { grid: webmercator, identifier: "OGC:1.0:GoogleMapsCompatible" }  # a grid may have several

# ---- output formats (encoder settings; layers choose which they offer) ----
output_formats:
  jpeg: { quality: 85, progressive: false }   # quality 1-100
  png: { compression: 6 }                     # zlib level 1-9
  webp: { quality: 80, lossless: false }
  geotiff: { compress: deflate, predictor: 2, tiled: true }   # none | deflate | lzw | zstd

# ---- layers (order is significant; advertised in this order) ----
layers:
  - name: ortho2024

    # Pyramid: which grids this layer is published in, and how deep.
    grids: [webmercator, rd_new_quad]
    max_resolution: native           # default: deepest level that reaches source's native res per grid
    # max_resolution: 0.5            # alternatively: cap at 0.5 m/pixel
    # bbox: [-2000, 290000, 294000, 630000]  # optional override; default = the source's own extent
    # bbox_crs: EPSG:28992           # in its native CRS. CRS of bbox (default EPSG:4326 lon/lat).
                                     # BoundingBoxes in this CRS are exact; other CRSes and the
                                     # WGS84 boxes are reprojected from it

    # Sources: list of GDAL-readable URIs. v1 uses the first; the list shape is for
    # forward-compat with multi-source composition.
    sources:
      - uri: "/vsiaz/photos/ortho_2024.vrt"   # GDAL dataset name, verbatim; relative paths
                                              # resolve against kwms's working directory
        credentials: pdok_blob       # references secrets.yaml; omit if no credentials needed
        # credentials_prefix: /vsiaz/photos/  # where the credentials apply; default: the
        #                                     # URI's own /vsiXXX/<bucket>/
        # open_options: [KEY=VALUE, …]        # GDAL open options, as `gdalinfo -oo` takes them

    # A tile index shipped next to kwms whose tiles live in a bucket (GDAL's GTI
    # driver). Tile resolutions (GSD, may differ per area) and overviews are
    # detected at startup; see source-abstraction.md.
    #
    # sources:
    #   - uri: "GTI:data/2026_orthoHR/tiles.shp"
    #     credentials: open_tiles
    #     credentials_prefix: /vsiaz/open-tiles/   # required: the URI is not a bucket path

    # Cache: mode + TTL + optional per-level overrides. The TTL counts from
    # when a tile was rendered, in every tier. For imagery that doesn't
    # change, a long TTL (e.g. 365d) keeps `kwms cache update` results.
    cache:
      mode: cache                    # cache | memory | passthrough (default: cache)
      max_level: auto                # deepest level stored durably (disk/Azure), per grid:
                                     # auto (default: levels whose tiles are larger than one
                                     # source file) | <level> | all. Deeper `cache` levels are
                                     # kept in memory only. See caching.md.
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
    # source_mask: data/other_mask.gpkg  # this layer's mask (overrides the global one); `none` = no mask
    # output_formats: { jpeg: { quality: 92 } }  # this layer's encoder settings for the listed formats
    # cache_control: "public, max-age=300"       # replaces server.cache_control.images for this layer
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
  cache_blob:                       # for the cache: opendal azblob option keys
    account_name: "tilecache"
    account_key: "..."               # or: sas_token: "sv=…&sig=…" (read/write/delete)
```

Source credentials are GDAL config options (`AZURE_STORAGE_ACCOUNT`, …); cache credentials are opendal options for the cache's backend (`account_name`, `account_key` / `sas_token` for `azblob`). They are merged into that tier's `options`, and an inline option there wins over the secret. `kwms config check` rejects a tier whose `credentials` block doesn't exist.

Each credentials block is a flat map of GDAL config options (for a read-only Azure SAS: `AZURE_STORAGE_ACCOUNT` plus `AZURE_STORAGE_SAS_TOKEN`, the query string without the leading `?`). At startup, before any source is opened, each block is registered with GDAL as **path-specific options** (`VSISetPathSpecificOption`) for the source's `credentials_prefix` — by default the `/vsiXXX/<bucket>/` of its URI. GDAL uses them for every file under that prefix and nowhere else, including files a dataset opens itself (the tiles of a GTI index or VRT) and reads on GDAL's own worker threads. So one server can host layers backed by different accounts without bleeding credentials across buckets. A prefix can carry only one credentials block, and `kwms config check` rejects references to blocks that don't exist.

### Cache tiers and environment variables

`cache.tiers` is a list, so — like `layers:` — it is replaced wholesale by a higher-precedence source, never merged item by item. To override it from the environment, give the whole list as a YAML flow value:

```sh
KWMS_CACHE__TIERS='[{type: memory, max_bytes: 256MB}, {type: store, backend: fs, options: {root: /tmp/kwms}}]'
```

### Where tiles are stored

Durable tiers store each tile at `<layer>/<epsg>/<format>/<z>/<x>/<y>.<ext>` under the tier's root, with a `.meta` sidecar holding its render time — e.g. `2026_orthoHR/28992/jpeg/6/30/32.jpeg`. The second segment is the grid's EPSG code (its config name for a CRS without one). The same tiles serve WMTS, TMS and WMS, since GetMap is composed from grid tiles. This puts two constraints on the config:

- **Layer and custom-grid names are directory names:** `A-Z a-z 0-9 _ - .`, at most 128 characters, not starting with `.`.
- **One grid per CRS per layer:** two grids of one layer in the same CRS would share a directory, so config validation rejects it.

Renaming a layer starts it with an empty cache. Changing a custom grid's definition under the same name requires clearing `<layer>/<epsg>/` yourself.

### Filling the cache: `kwms cache update`

```sh
kwms cache update                      # all layers, grids and formats; levels 0..auto
kwms cache update --layer 2026_orthoHR --grid rd_new_quad --format jpeg --dry-run
kwms cache update --max-level 8 --force
```

Renders tiles into the durable tiers ahead of requests, down to each layer's `cache.max_level` (`--max-level auto`, the default). With `max_level: auto` that is every level whose tiles are larger than one source file, measured from a GTI tile index. Those tiles open several source files and are slow to render on demand. Smaller tiles open one to four files through their own overviews, so storing them buys little, and the server keeps them in memory only. For the 2026 ortho (10 km COGs) that is RD levels 0–6 and Web Mercator levels 0–11. A numeric `--max-level` can go less deep, but not deeper than the layer's limit. `--dry-run` prints the plan with tile counts.

Fresh tiles are skipped (unless `--force`), so re-running resumes an interrupted run and retries failed tiles. Timeouts are relaxed compared to serving: the server's `request_timeout` doesn't apply; each attempt at a tile may take `--tile-timeout` (default 10m); waits for a free dataset or render slot are retried without counting as failures; other errors are retried `--retries` times (default 3) with backoff. Other flags: `--min-level`, `--concurrency` (default 4), `--max-tiles` (default 1 000 000, a guard against typos in `--max-level`).

### Publishing like PDOK

The shipped `config.yaml` publishes the WMTS exactly like PDOK's [luchtfotorgb service](https://service.pdok.nl/hwh/luchtfotorgb/wmts/v1_0?request=GetCapabilities), so a client configured for PDOK only needs the host swapped. That means:

- the same service metadata;
- the endpoint path `/hwh/luchtfotorgb/wmts/v1_0`;
- tile URLs `…/v1_0/2026_orthoHR/EPSG:28992/07/63/65.jpeg`;
- the same seven TileMatrixSets: `EPSG:28992`, `EPSG:3857`, `EPSG:4258`, `EPSG:4326`, `EPSG:25831`, `EPSG:25832` and `OGC:1.0:GoogleMapsCompatible`;
- zero-padded TileMatrix ids and `EPSG:` CRS ids.

The four Dutch-extent grids (`nl_4258`, `nl_4326`, `nl_25831`, `nl_25832`) are custom grids with a bounded `extent`, derived from PDOK's document: the level-0 scale gives the base resolution, the deepest matrix size gives the extent.

Checked against PDOK's document:

- identical service metadata, operations, layer links, TileMatrixSet order, identifiers, level counts and matrix sizes;
- scales and corners equal to floating-point precision.

The same tile URL fetched from both returns the same picture in all seven sets (correlation 0.96–0.99). In Web Mercator, PDOK's tiles sit one pixel off a direct warp of the source; kwms's don't.

Changing encoder settings or TileMatrixSet definitions changes what tiles look like: refresh the durable cache (`kwms cache update --force`) afterwards.

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
