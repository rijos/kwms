# Fuzzing kwms request handling

Five [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) (libFuzzer) targets
drive every way untrusted input enters the server:

| Target | Input |
|---|---|
| `kvp` | raw query string → `protocol::kvp::Kvp` |
| `wms` | WMS query string → stage 1 + stage 2 |
| `wmts_kvp` | WMTS KVP query string → stage 1 + stage 2 |
| `wmts_rest` | path below `/wmts/` → stage 1 + stage 2 |
| `tms` | path below `/tms` → stage 1 + stage 2 |

All of them call `kwms::protocol::fuzz_harness`, which asserts the invariants
(no panic; accepted requests reference only catalog entries and in-limit
values; every rejection renders as well-formed, printable-ASCII XML in all
three protocols' error formats). The same harness runs as property tests on
stable in every `cargo test`.

## Running

Needs a nightly toolchain and cargo-fuzz:

```sh
rustup toolchain install nightly --profile minimal
cargo install cargo-fuzz --locked
```

Then, from `fuzz/` (`--features bindgen` and `LIBCLANG_PATH` are only needed
when the system GDAL is newer than gdal-sys's prebuilt bindings):

```sh
cargo +nightly fuzz run wms corpus/wms seeds/wms --features bindgen -- \
    -dict=ogc.dict -max_len=8192 -max_total_time=600
```

`seeds/<target>/` holds hand-written valid requests (committed); libFuzzer
writes new interesting inputs to `corpus/<target>/` and crashing inputs to
`artifacts/<target>/` (both git-ignored). Reproduce a crash with
`cargo +nightly fuzz run <target> artifacts/<target>/<file>`.

## Notes

- `Cargo.lock` here is seeded from the main lockfile so the fuzzers exercise
  the same dependency versions as the server. After changing dependencies in
  the main crate, re-copy it: `cp ../Cargo.lock .`.
- Fuzzing builds (`cfg(fuzzing)`) leave out the `server`, `subcommand` and
  `cli` modules. The targets don't use them, and nightly rustc (2026-11-17)
  fails to prove the axum handler futures `Send` in release mode — a
  compiler regression; stable builds them fine.
