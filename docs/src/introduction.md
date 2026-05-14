# Introduction

kwms is a WMS/WMTS/TMS webserver in Rust.

Core requirements:

- All configuration settings are defined in `config.yaml` (with `secrets.yaml` merged in).
- Hybrid caching scheme: users can define per-layer whether tiles are cached on-the-fly or always fetched from source.
- Source backends are abstracted behind a trait; the reference implementation uses GDAL with VSI prefixes (`vsiaz://`, `vsicurl://`, …) and VRT support.
