# Request validation

Every WMS, WMTS and TMS request passes through two explicit stages in the
`protocol` module before anything else in the server sees it. Only the
validated output of stage 2 reaches the pipeline, the cache, or GDAL.

```
raw query / path ──► stage 1: syntax ──► stage 2: rules vs Catalog ──► validated type ──► pipeline
                     (request.rs)        (validate.rs)                  WmsGetMap / TileQuery
```

## Principles

- **GDAL only ever sees values from the config.** A request can *select*
  configured layers, grids and CRSes; it can never *supply* a value that
  GDAL parses. Request CRSes are resolved by string lookup in the
  [`Catalog`](#the-catalog); GDAL is consulted for CRSes only at startup.
- **Validated types can't be forged.** `WmsGetMap` and `TileQuery` have
  private fields and are constructed only by the validators, so the compiler
  guarantees the pipeline never receives unvalidated input.
- **Error responses contain nothing from the request.** `OgcError` messages
  are `&'static str` literals and locators are a closed `Param` enum, so an
  exception document is assembled only from text written in this codebase —
  no echoed values, markup, control characters or bidi marks, whatever the
  client sends. The request id in internal-error responses is always one the
  server generated; a client's `X-Request-Id` is only logged.

## Stage 0: query-string decoding (`protocol/kvp.rs`, WMS + WMTS)

| Rule | Error |
|---|---|
| query ≤ 8 KiB, ≤ 64 parameters, names ≤ 64 bytes, values ≤ 4 KiB | `InvalidParameterValue` |
| `+` and `%XX` decoded exactly once; malformed escapes or non-UTF-8 rejected | `InvalidParameterValue` |
| keys are case-insensitive; empty keys rejected | `InvalidParameterValue` |
| the same key twice (any case or encoding) rejected | `InvalidParameterValue` |
| unknown keys accepted and ignored (OGC vendor parameters) | — |

## WMS (`protocol/wms/`)

Stage 1 checks presence and shape: `VERSION` 1.1.1/1.3.0, `SRS` in 1.1.1 vs
`CRS` in 1.3.0, `BBOX` = four finite numbers with min < max, `WIDTH`/`HEIGHT`
positive integers (digits only), known `FORMAT`, `TRANSPARENT` TRUE/FALSE,
`BGCOLOR` 0xRRGGBB. Stage 2:

| Rule | Error (locator) |
|---|---|
| GetFeatureInfo is not implemented | `OperationNotSupported` (REQUEST) |
| `LAYERS` names exactly one layer | `InvalidParameterValue` (LAYERS) |
| the layer is configured | `LayerNotDefined` (LAYERS) |
| `STYLES` empty, or one `default`/empty entry per layer | `StyleNotDefined` (STYLES) |
| `CRS`/`SRS` advertised for the layer (any OGC spelling) | `InvalidCRS` / `InvalidSRS` (CRS / SRS) |
| `WIDTH`, `HEIGHT` ≤ `server.max_request_pixels` | `InvalidParameterValue` (WIDTH / HEIGHT) |
| BBOX geometry is renderable (below) | `InvalidParameterValue` (BBOX) |
| `FORMAT` offered by the layer | `InvalidFormat` (FORMAT) |

The BBOX is converted to x/y with the axis order the catalog recorded for the
CRS (WMS 1.3.0 only). A BBOX entirely outside the grid renders a blank image.

**BBOX geometry.** Four finite numbers aren't enough: the output pixel grid
they define must be one GDAL can georeference faithfully. Per axis, the pixel
size `(max − min) / WIDTH|HEIGHT`

- must be finite — `-1e308,…,1e308` has finite corners but an infinite
  width (it used to return a GeoTIFF with a `NaN`/`inf` geotransform);
- must be ≥ the grid's finest resolution ÷ 1024
  (`validate::MAX_OVERSAMPLING`) — no client needs to zoom in further;
- must be ≥ 2¹⁶ float steps of the largest coordinate, so neighbouring pixel
  edges stay distinct (a subnormal extent made GDAL fail with a 500; a
  sub-precision one returned a silently blank image).

Behind this, `bbox_to_geotransform` refuses any non-finite or non-invertible
geotransform as an internal error, so a gap here can't reach GDAL.
Errors are `<ServiceExceptionReport>`s with HTTP 200 (WMS convention).

## WMTS (`protocol/wmts/`)

REST paths must match the advertised template exactly:
`/wmts/{Layer}/{TileMatrixSet}/{TileMatrix}/{TileCol}/{TileRow}.{ext}` (or
`/wmts/1.0.0/WMTSCapabilities.xml`). Indices are plain digits (leading zeros
allowed). Stage 2:

| Rule | Error (locator) |
|---|---|
| GetFeatureInfo is not implemented | `OperationNotSupported` (REQUEST), HTTP 400 |
| `LAYER` is configured | `InvalidParameterValue` (LAYER) |
| `STYLE`, if given, is `default` or empty | `InvalidParameterValue` (STYLE) |
| `TILEMATRIXSET` is one the layer is published on (or a legacy alias) | `InvalidParameterValue` (TILEMATRIXSET) |
| TileMatrix / TileCol / TileRow inside the TileMatrixSet | `TileOutOfRange` (TILEMATRIX / TILECOL / TILEROW) |
| format offered by the layer | `InvalidParameterValue` (FORMAT) |

Errors are OWS `ExceptionReport`s with the HTTP statuses of WMTS 1.0.0, except `OperationNotSupported`: 400 instead of 501, so request errors are never 5xx.

## TMS (`protocol/tms/`)

Paths: `/tms/1.0.0[/{layer}[/{z}/{x}/{y}.{ext}]]`. Stage 2 checks the layer
exists (`LayerNotDefined`, 404), `z` and the bottom-up `y` are inside the
layer's first grid (`TileOutOfRange`, 404), then flips `y` to top-down and
applies the shared tile rules.

## The catalog

`protocol/catalog.rs`, built once at startup from the validated config:
layers with their grids and formats, the request size limit, and a CRS table
mapping every accepted spelling (`EPSG:n` in any case, `urn:ogc:def:crs:…`,
`http(s)://www.opengis.net/def/crs/…`) to the config's own CRS token and its
GDAL-reported axis order. A configured CRS GDAL can't parse stops startup.

## Testing

- Each rule has a unit test next to it.
- `protocol/fuzz_harness.rs` defines the invariants (no panic; accepted
  requests reference only catalog entries and in-limit values; every
  rejection renders as well-formed printable-ASCII XML in all three error
  formats). Property tests run it on stable in every `cargo test`, including
  generators weighted toward valid requests and a check that they actually
  reach the accepted path.
- The same harness backs five libFuzzer targets in `fuzz/` — see
  `fuzz/README.md`.
- `pipeline::wms_assemble::tests::hostile_bboxes_never_cause_internal_errors`
  goes one step further for WMS: random BBOXes (arbitrary floats, ±inf, NaN,
  subnormals, ±`f64::MAX`, values a hair off grid and tile edges) and sizes
  run through validation *and* `serve_get_map`; every outcome must be an
  image of the requested size (with a finite geotransform) or a client
  error — never a 500. It checks that at least a quarter of the cases reach
  the renderer, and fails within one run if the BBOX geometry rule is
  removed.
