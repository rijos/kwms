# Testing

The repository has two layers of tests, mirroring Rust convention:

- **Unit tests** live alongside the code in `src/**/foo.rs` under `#[cfg(test)]` modules. They exercise pure functions (tile math, KVP parsing, axis-order rules, config merge, level-selection).
- **Integration tests** live in `tests/` at the repo root and exercise the full server end-to-end. Each `.rs` file under `tests/` is a separate test binary.

See [Module layout](./module-layout.md) for the directory tree, including the `tests/` structure.

## Capabilities rendering (pdok-XML tests)

`tests/capabilities.rs` covers GetCapabilities generation for all four protocol flavors. Each test loads a fixture config, asks the protocol layer to render the capabilities document, and asserts the output matches a pdok XML file (with insignificant whitespace and attribute order normalized before diffing).

### Fixture configs

`tests/fixtures/config/` holds small `config.yaml` + `secrets.yaml` pairs designed to exercise specific scenarios:

- **`pdok_multigrid.yaml`** — a PDOK-style multi-grid layer with `grids: [webmercator, rd_new_quad]` and several aerial-year layers. Exercises CRS-per-grid mapping, per-CRS BoundingBox transformation via PROJ, and per-grid level-count derivation (webmercator gets ~22 levels, rd_new_quad gets ~20 — same data, different grid base scales).
- **`single_grid_minimal.yaml`** — one layer, one well-known grid, defaults for everything else. Baseline for "does the simplest possible config produce a valid document."
- **`max_resolution_capped.yaml`** — same as the minimal fixture but with `max_resolution: 0.5` to verify depth-capping shortens the advertised TileMatrix list.
- **`flags_off.yaml`** — a layer with `queryable: false` and `opaque: false`, to confirm both flags toggle the corresponding XML attributes.
- **`custom_grid.yaml`** — declares a custom grid in the top-level `grids:` section and references it from a layer. Verifies the custom-grid path through capabilities rendering.

### Test cases per fixture

For each fixture, the test file asserts:

- **WMS 1.1.1**: output matches `tests/fixtures/pdok/capabilities/wms_111_<fixture>.xml`. Axis order in BoundingBox elements is x,y for all CRSes (WMS 1.1.1 convention).
- **WMS 1.3.0**: output matches `wms_130_<fixture>.xml`. Geographic CRSes use lat,lon axis order in BoundingBox elements (1.3.0 convention); projected CRSes stay x,y. The 1.1.1 vs 1.3.0 difference is baked into separate pdok files, not computed at test time.
- **WMTS 1.0.0**: output matches `wmts_<fixture>.xml`, including the full `TileMatrixSet` definitions derived from each grid + `max_resolution`. Verifies scale denominators, top-left corners, tile matrix counts.
- **TMS 1.0.0**: root document (`/tms/1.0.0/`) and per-layer documents (`/tms/1.0.0/{layer}/`) match `tms_root_<fixture>.xml` and `tms_<layer>_<fixture>.xml`.

### Updating pdok files

Pdok XML files are checked into the repo. Update workflow: set `UPDATE_PDOK=1` and re-run the failing test; the test harness writes the produced XML over the pdok file. CI runs without the env var, so accidental drift fails the build. (The name is deliberately unprefixed: the loader's `KWMS_*` env-var override path would otherwise inject the variable into the config tree.)

### Schema validation (future)

Validating output against the published OGC XSDs (WMS 1.1.1, WMS 1.3.0, WMTS 1.0.0) would catch structural errors that text-diff pdok files miss. Deferred: Rust's XSD tooling is thin, and for v1 the combination of pdok diffs + cross-checking against PDOK in early iterations is enough.

## Other test suites

- **`tests/tile_render.rs`** — pdok-tile diffs. A fixture directory of source files (small COGs, VRTs) plus expected output tiles per layer/format/level. Tests render the tiles and compare bytes; for lossy formats (JPEG, WebP-lossy), compare via a perceptual-hash threshold.
- **`tests/conformance.rs`** — protocol conformance against GetMap, GetTile, and GetFeatureInfo with known inputs. Asserts content types, response headers, axis order per WMS version, and ServiceException response shapes for error cases.
- **`tests/cache_integration.rs`** — end-to-end exercises of the hot tier, persistent tier (against a temp-dir OpenDAL backend), TTL expiry, passthrough mode, level-rule overrides, and single-flight coalescing under concurrent load.
- **`tests/config_merge.rs`** — config loading, deep-merge semantics, env-var overrides, CLI overrides, and JSON Schema export.
- **Property-based tests** via [`proptest`](https://crates.io/crates/proptest) for tile↔bbox conversions, level selection from arbitrary bboxes/sizes, and the YAML merge function. Catches off-by-one and edge-case bugs that example-based tests miss. Live as `#[cfg(test)]` modules next to the code under test.
