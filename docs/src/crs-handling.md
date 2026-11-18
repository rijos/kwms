# CRS handling

> **PROJ network access is off.** At startup kwms calls
> `OSRSetPROJEnableNetwork(FALSE)` (`crs::disable_proj_network`) and refuses
> to start if GDAL doesn't confirm it, overriding `PROJ_NETWORK` and
> `proj.ini`. With network access on, a transform inside a request could
> block on a download from cdn.proj.org. Transformations use only locally
> installed grids — install the ones you need (e.g. `rdtrans2018.tif` for
> RDNAPTRANS™) with the PROJ data package.

All CRS knowledge — EPSG resolution, axis-order rules, geographic vs projected classification, bbox transforms — is **delegated to GDAL/PROJ**. kwms does not maintain its own CRS database or transform math. Treating CRS as a black-box capability of GDAL keeps us out of the long tail of projection edge cases that GDAL/PROJ already solve.

The `crs` module exposes free functions (`is_geographic`, `native_axis_order`, `transform_bbox`, `validate`, …) over a lightweight `Crs` newtype that the rest of the codebase passes around as an opaque token. Inside the module, all calls bottom out in GDAL/PROJ via the `gdal` crate. No trait abstraction — GDAL is a load-bearing dependency of the whole project, not something to be hidden.
