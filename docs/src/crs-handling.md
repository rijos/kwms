# CRS handling

All CRS knowledge — EPSG resolution, axis-order rules, geographic vs projected classification, bbox transforms — is **delegated to GDAL/PROJ**. kwms does not maintain its own CRS database or transform math. Treating CRS as a black-box capability of GDAL keeps us out of the long tail of projection edge cases that GDAL/PROJ already solve.

The `crs` module exposes free functions (`is_geographic`, `native_axis_order`, `transform_bbox`, `validate`, …) over a lightweight `Crs` newtype that the rest of the codebase passes around as an opaque token. Inside the module, all calls bottom out in GDAL/PROJ via the `gdal` crate. No trait abstraction — GDAL is a load-bearing dependency of the whole project, not something to be hidden.
