# Per-layer behavior details

## Reprojection

- Triggered whenever the request CRS differs from the source's native CRS.
- Performed inside the GDAL source implementation via GDAL's warp API.
- Each layer declares a single resampling kernel applied uniformly to all reprojection and level-driven resampling (no separate up/down kernels in v1). Tokens are delegated to GDAL's `ResampleAlg::from_str`: `nearestneighbour`, `bilinear`, `cubic`, `cubicspline`, `lanczos`, `average`, `mode`. Default: `bilinear`. (`gauss` is rejected: every read goes through the GDAL warper, which has no Gauss kernel.) All reads — same-CRS or reprojected, tiles and WMS composites — use `GDALWarp` with this kernel and GDAL's automatic overview selection.

## Bounding box & out-of-coverage requests

- A layer's extent defaults to its source's own extent in the source's native CRS: the footprints of a tile index, or a raster's extent. Operators may override it with `bbox` (plus `bbox_crs`, default WGS84 lon/lat). Capabilities advertise it exactly in that CRS. The WGS84 boxes and the BoundingBoxes in the other advertised CRSes are reprojected from it via GDAL/PROJ, with densified edges.
- Requests whose bbox extends beyond the layer's coverage are served with the overlap rendered normally and the out-of-coverage area filled as **no data**:
  - **Formats with alpha** (PNG, WebP, GeoTIFF): transparent; an alpha band is written.
  - **JPEG**: white.
- Fully-out-of-coverage requests return a fully no-data image, not an error. This matches typical WMS client expectations.

### Where a layer has data

A pixel has data only when all of these hold (`source::gdal::mask`):

1. **It lies inside the source's coverage.** For a tile index, that is the actual footprint polygons of its tiles, so gaps between tiles (open sea) are no data rather than black. For a single raster it is the raster's extent.
2. **It lies inside the layer's `source_mask`, if one is set.** The mask is a polygon vector dataset (shapefile, GeoPackage, …), and every polygon of every feature counts. The top-level `source_mask` applies to every layer; a layer's own `source_mask` overrides it, and `source_mask: none` turns a global one off for that layer. The shipped `data/source_mask.shp` covers everything except Belgium, so the Belgian parts of border tiles are hidden.
3. **It isn't the nodata value** (below).

GTI returns pure black where it has no tile, so the edges of the coverage need two safeguards against thin grey lines on no-data:

- **No black in interpolation.** For a tile index, reads treat pure black (0 in every band) as "no source pixel" (`UNIFIED_SRC_NODATA`), so the warp never blends the gap between tiles into edge pixels, however far zoomed out. Output nodata stays unset, so a genuinely black pixel is still drawn black.
- **The outline is trimmed.** The tile footprints are snapped to 1/100 of the finest GSD, so touching tiles really touch (the 2026 index has edges at 480000 and 480000.000000051), and dissolved into one outline at startup. The pixels that outline passes through count as no data, because at coarse scales GTI snaps tile edges to its overview grid, up to half an overview pixel off the true footprint. Seams between neighbouring tiles and the edges of a `source_mask` aren't trimmed.

Deciding by geometry rather than pixel value keeps genuinely black pixels, such as deep shadow, visible. The areas are reprojected once per CRS and rasterized per image at its exact size, which costs about 2 ms per 256 px tile. An image with no data anywhere is never cached.

Changing a mask changes what cached tiles should look like: clear the layer's durable cache, or run `kwms cache update --force`.

## NoData

- Defaults to the source's NoData as reported by GDAL.
- Optionally overridden per layer (single scalar; per-band lists are not implemented yet). The override is applied as the source nodata when warping. Useful when the source omits NoData metadata or when additional sentinel values should be masked.
- Pixels whose every band equals NoData are no data: transparent or white, like areas outside the coverage.
