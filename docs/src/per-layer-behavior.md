# Per-layer behavior details

## Reprojection

- Triggered whenever the request CRS differs from the source's native CRS.
- Performed inside the GDAL source implementation via GDAL's warp API.
- Each layer declares a single resampling kernel applied uniformly to all reprojection and level-driven resampling (no separate up/down kernels in v1). Allowed kernels: `nearest`, `bilinear`, `cubic`, `cubic_spline`, `lanczos`, `average`, `mode`. Default: `bilinear`.

## Bounding box & out-of-coverage requests

- A layer's bbox is a single WGS84 box. It defaults to the source's native bbox transformed to WGS84; operators may override it in config. Per-CRS BoundingBox elements advertised in GetCapabilities are computed by transforming this WGS84 bbox into each advertised CRS via GDAL/PROJ.
- Requests whose bbox extends beyond the layer's coverage are served with the overlap rendered normally and the out-of-coverage area filled:
  - **RGBA outputs** (PNG/WebP-alpha): transparent.
  - **Opaque outputs** (JPEG, WebP-lossy without alpha): the layer's declared background color (default black).
  - **Raw / float outputs** (GeoTIFF, raw float): the layer's NoData value.
- Fully-out-of-coverage requests return a fully-filled tile (transparent / background / NoData), not an error. This matches typical WMS client expectations.

## NoData

- Defaults to the source's NoData as reported by GDAL.
- Optionally overridden per layer (single scalar or per-band list). Useful when the source omits NoData metadata or when additional sentinel values should be masked.
