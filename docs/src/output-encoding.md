# Output encoding

All tile and image encoding goes through GDAL. The same crate that drives source reads handles the writer side via GDAL's raster driver system. No `Encoder` trait, no per-format submodule — one function, four formats.

## Approach

For each rendered tile, the pipeline holds:

- A pixel buffer (rows × cols × bands, at the appropriate dtype).
- Optional georeferencing (CRS + affine geotransform) — populated for GeoTIFF / raw float output, ignored for PNG / JPEG / WebP.
- NoData is set on the output bands for every format: GeoTIFF records it, PNG writes it as a `tRNS` chunk (nodata pixels become transparent), JPEG has no equivalent and ignores it.

To encode:

1. Wrap the pixel buffer as an in-memory GDAL `MEM` dataset.
2. Apply georeferencing tags if present.
3. `CreateCopy` against the target driver (`PNG`, `JPEG`, `WEBP`, `GTiff`) into `/vsimem/<random>` with format-specific options.
4. Read the bytes back from `/vsimem/` and unlink the virtual file.

The whole encoder lives in one file:

```rust
// src/encode.rs
pub fn encode(
    buffer: &PixelBuffer,
    georef: Option<&GeoRef>,
    format: OutputFormat,
    opts: &EncodeOpts,
) -> Result<Bytes>;
```

`OutputFormat` is a small enum (`Png | Jpeg | Webp | GeoTiff`). `EncodeOpts` is a per-format options struct that flattens to GDAL's `KEY=VALUE` option strings at call time.

## Format options

| Format | GDAL driver | Notable options |
|---|---|---|
| PNG | `PNG` | `ZLEVEL` (1–9 compression) |
| JPEG | `JPEG` | `QUALITY` (1–100), `PROGRESSIVE` |
| WebP | `WEBP` | `QUALITY`, `LOSSLESS=TRUE` |
| GeoTIFF | `GTiff` | `COMPRESS` (`DEFLATE` / `LZW` / `ZSTD`), `PREDICTOR`, `TILED`, `BLOCKXSIZE`/`BLOCKYSIZE` |

Defaults are tuned for web-tile delivery: PNG `ZLEVEL=6`, JPEG `QUALITY=85`, WebP `QUALITY=80` lossy. GeoTIFF defaults to `COMPRESS=DEFLATE PREDICTOR=2 TILED=YES`. Per-layer config can override defaults.

## MIME types & extensions

| Format | MIME type | Extension |
|---|---|---|
| PNG | `image/png` | `.png` |
| JPEG | `image/jpeg` | `.jpeg` |
| WebP | `image/webp` | `.webp` |
| GeoTIFF | `image/tiff; application=geotiff` | `.tif` |

Used for `Content-Type` response headers and for WMTS RESTful URL extension routing.

## Why GDAL for all four

The alternative — `image` for PNG/JPEG/WebP plus a GDAL path for GeoTIFF — splits encoding into two code paths and re-introduces the abstraction we'd then need to invent. GDAL already has the source's georeferencing in hand (`source/gdal/` opened it), so handing the same metadata to the writer side is essentially free.

GDAL's web-raster encoders run 10–30% slower than dedicated Rust crates (`png`, `jpeg-encoder`, `image-webp`) for 256×256 tiles in microbenchmarks. Irrelevant next to reprojection cost on the hot path, and trivially compensated by [caching](./caching.md).
