# kwms

WM(T)S server for aerial imagery, written in Rust.

The full design document lives in [`docs/`](./docs/src/SUMMARY.md). Render it locally with:

```sh
cargo install mdbook
mdbook serve docs   # http://localhost:3000
```

## Prerequisites

GDAL/PROJ system libraries and a Rust toolchain.

### Debian / Ubuntu

```sh
sudo apt-get update
sudo apt-get install -y libgdal-dev libproj-dev pkg-config build-essential libclang-dev
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
```

### macOS

```sh
brew install gdal proj
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"
```

### Other platforms

GDAL is available via conda or OSGeo4W on Windows. Rust install instructions: <https://www.rust-lang.org/tools/install>.

## Build

```sh
cargo build
cargo test
```
