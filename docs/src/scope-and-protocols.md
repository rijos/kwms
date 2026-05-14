# Scope & protocols

## Architecture

Internally the server is a **tile engine** (z/x/y addressing). WMS is a thin layer on top: WMS GetMap requests are decomposed into the tiles that cover the requested bbox, those tiles are fetched/rendered through the same pipeline as WMTS/TMS, and reassembled (with reprojection/resampling) into the response image. This keeps caching, source access, and reprojection unified.

## Protocols & versions

- **WMS 1.3.0** and **WMS 1.1.1** — both required. Handle the axis-order difference for geographic CRSes between the two versions.
- **WMTS 1.0.0** — KVP and RESTful bindings.
- **TMS 1.0.0** — OSGeo TMS (y-flipped from XYZ; document the flip clearly).

## Output formats

- **PNG** (8-bit RGBA) — default for web map output
- **JPEG** — opaque imagery, no alpha
- **WebP** — lossy and lossless variants
- **Raw float / GeoTIFF** — for elevation and scientific rasters; preserves source pixel values without 8-bit quantization

Per-layer config declares which formats the layer advertises; requests for unsupported formats return a protocol-appropriate error. All four formats share one encoder — see [Output encoding](./output-encoding.md).

## GetFeatureInfo

Supported. Returns the source pixel value(s) at the queried location (all bands, in source units, before any styling/quantization). The [`Source`](./source-abstraction.md) trait exposes a `sample(coord, crs) -> Vec<PixelValue>` method used by the GetFeatureInfo handler.
