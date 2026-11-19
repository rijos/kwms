# Unsafe code audit

kwms is internet-facing. Unsafe code is treated with extreme prejudice:

- **One module.** `unsafe` is denied crate-wide (`#![deny(unsafe_code)]`)
  and allowed only in `src/ffi.rs`, which exposes safe functions. Nothing
  else in the crate contains `unsafe`.
- **One operation per block.** `clippy::multiple_unsafe_ops_per_block` is
  denied, so each entry below is exactly one call or pointer operation.
- **Documented twice.** Each block has a `// SAFETY:` comment
  (`clippy::undocumented_unsafe_blocks` is denied) and an
  `// UNSAFE-AUDIT: <id>` tag pointing here.
- **Kept in sync mechanically.** `tests/unsafe_audit.rs` fails the build if
  `unsafe` appears outside `src/ffi.rs`, if a block lacks a tag, if a tag has
  no entry in this document, or if an entry has no tag in the code.
- **No client input reaches any of these calls.** They operate on datasets
  and CRSes built from the configuration; request values reach GDAL only as
  numbers already validated by the protocol layer (bbox, size), never as
  strings.

Every claim about GDAL below was checked against the GDAL **3.11.4** sources
(the version kwms runs against); line numbers refer to that tag. Claims about
the `gdal` crate refer to version 0.17.1.

## Threading

GDAL is re-entrant but not thread-safe. The full analysis is in
[Concurrency](./concurrency.md); in short:

- **No `ffi` call runs on a tokio runtime worker while requests are
  served.** Calls happen during startup (before the listener is bound), on
  the startup thread, the dataset-pool workers, the blocking pool, or test
  threads. Each entry below has a **Threads** field.
- **No object passed to an `ffi` call is shared between threads.** Each
  dataset or `SpatialRef` is owned by, or local to, the calling thread for
  the whole call.
- **The CPL error state is per thread,** and each reset-then-read pair runs
  inside one synchronous `ffi` function, never across an `.await`.

## Global side effects of GDAL calls

These apply to every entry and are not repeated per entry.

- **Thread-local CPL error context.** GDAL keeps the last error (type,
  number, message) per thread. The context is allocated lazily by GDAL on
  first use, owned by GDAL and freed at thread exit
  (`port/cpl_error.cpp`). kwms resets it (F1) before calls whose error it
  reports, so a message always belongs to the current call.
- **GDAL's default error handler writes every `CPLError` to stderr**
  (`cpl_error.cpp:41`, `CPLDefaultErrorHandler`). These lines can contain
  file paths or source URIs. They go to the server's stderr only — never
  to clients — but they bypass the JSON log format.
- **PROJ context.** CRS operations use a per-thread PROJ context and read
  the PROJ database (`proj.db`) from disk.
- **GDAL block cache.** Raster I/O goes through GDAL's process-wide block
  cache (`GDAL_CACHEMAX`).
- **PROJ network access** is switched off process-wide at startup (F16,
  confirmed by F17), so no transform downloads grids.
- **Source credentials** are registered process-wide, per object-store path
  prefix, at startup (F18) and never changed afterwards.
- **Allocation failure inside GDAL** either aborts the process (`CPLMalloc`,
  `CPLCalloc`) or throws `std::bad_alloc`. Where GDAL does not catch it, a
  C++ exception unwinding into Rust through the C ABI is undefined behaviour
  that Rust cannot prevent. Mitigation: request limits keep each call's
  allocations small; on memory exhaustion the expected outcome is a process
  restart.

---

## F1 — `CPLErrorReset`

- **Location:** `ffi::cpl_error_reset`.
- **Threads:** any GDAL-using thread (startup, dataset-pool workers, blocking pool). Thread-local: it clears only the calling thread's state. It is called in the same synchronous function as the read that follows it (never across an `.await`), so the read can't land on another thread.
- **Purpose:** clear this thread's error state before a call whose error
  message we will report.
- **Arguments / memory passed:** none.
- **Allocates:** may allocate this thread's CPL error context on first use
  (owned by GDAL, freed at thread exit); in the "predefined context" branch
  it stores a TLS pointer (`cpl_error.cpp:809-823`). Nothing is returned to
  us.
- **Frees:** nothing of ours.
- **Side effects:** clears the thread's last error number, type and message
  (`cpl_error.cpp:825-833`).
- **Failure behaviour:** none; returns `void`. If no context can be obtained
  it returns without effect.
- **Verified by:** `raster::tests::last_cpl_error_only_reports_the_current_call`.

## F2 — `CPLGetLastErrorType`

- **Location:** `ffi::cpl_error_type` (used by `cpl_failed`,
  `cpl_error_message`).
- **Threads:** any GDAL-using thread. Reads only the calling thread's state; errors on other threads are invisible (`raster::tests::cpl_error_state_is_isolated_per_thread`).
- **Arguments / memory passed:** none.
- **Allocates / frees:** nothing (beyond lazily creating the context, see F1).
- **Returns:** an enum value by copy; `CE_None` if there is no context
  (`cpl_error.cpp:930-937`).
- **Side effects:** none (read-only).
- **Failure behaviour:** none.

## F3 — `CPLGetLastErrorMsg`

- **Location:** `ffi::cpl_error_message`.
- **Threads:** any GDAL-using thread. The buffer is the calling thread's own, so no other thread can modify or reallocate it.
- **Arguments / memory passed:** none.
- **Returns:** a pointer to either a static `""` or this thread's message
  buffer `szLastErrMsg` (`cpl_error.cpp:954-961`). The buffer is owned by
  GDAL. It is **only valid until the next CPL call on this thread**: with
  `CPL_ACCUM_ERROR_MSG=ON` a later error can `CPLRealloc` the whole context
  (`cpl_error.cpp:385-400`). GDAL never returns NULL; kwms checks anyway.
- **Allocates / frees:** nothing.
- **Side effects:** none.
- **How kwms uses it:** only when F2 reports an error, and the pointer goes
  straight to F4 with no CPL call in between.

## F4 — `CStr::from_ptr` on the CPL message

- **Location:** `ffi::cpl_error_message`.
- **Threads:** same thread and same synchronous call as F3.
- **Rust contract:** the pointer must be non-NULL, NUL-terminated, valid for
  reads up to and including the NUL, and not mutated while the `CStr` lives.
- **Why it holds:** non-NULL is checked; CPL always NUL-terminates the
  buffer (default size 500 bytes, `cpl_error.cpp:44-48`); the buffer is
  thread-local, so no other thread can modify it; the `CStr` is converted to
  an owned `String` immediately, before any other CPL call.
- **Allocates:** the owned `String` (Rust heap, truncated to 512 chars).
- **Information flow:** the text can contain file paths, source URIs or
  config values. It is placed only in *internal* errors, which
  `server::routes::error_reply` logs and replaces with a generic message
  carrying a server-generated request id; it is never sent to clients
  (tested by `server::tests::internal_errors_are_not_leaked_to_clients`).

## F5 — `GDALWarpAppOptionsNew`

- **Location:** `ffi::WarpAppOptions::new`.
- **Threads:** dataset-pool workers (source reads) and blocking-pool threads (WMS composite). The argument list and the resulting options object are local to one call on one thread.
- **Arguments / memory passed:**
  - `papszArgv`: `CslStringList::as_ptr()` — a NULL-terminated `char**`
    owned by the Rust caller, borrowed for the call. GDAL reads and copies
    the strings and keeps no pointer to them.
  - `psOptionsForBinary`: NULL.
- **Allocates:** on success, a `GDALWarpAppOptions` built with
  `std::make_unique` (`gdalwarp_lib.cpp:6322-6325`) whose ownership passes to
  the caller → wrapped in `WarpAppOptions` (non-NULL by construction).
- **Frees:** on failure GDAL frees everything it allocated (the
  `unique_ptr` goes out of scope).
- **Failure behaviour:** invalid arguments or any `std::exception`, including
  `std::bad_alloc`, are caught; GDAL raises `CPLError(CE_Failure)` and returns
  NULL (`gdalwarp_lib.cpp:6457-6461`). NULL therefore means "nothing to free";
  kwms turns it into an error with the CPL message.
- **Side effects:** CPL error state and a stderr line on failure.
- **Verified by:** `raster::tests::rejected_options_allocate_nothing_and_report_an_error`.

## F6 — `GDALWarpAppOptionsFree`

- **Location:** `Drop for ffi::WarpAppOptions`.
- **Threads:** the same thread that created the options (dropped at the end of `raster::warp`).
- **Arguments / memory passed:** the non-NULL pointer from F5.
- **Frees:** `delete psOptions` (`gdalwarp_lib.cpp:6476-6479`).
- **Exactly once:** `Drop` runs once, on every path including early return
  and unwinding; nothing else frees it. `GDALWarp` copies the options at
  entry (`gdalwarp_lib.cpp:1437-1440`), so freeing them after the call is
  safe.
- **Side effects:** none.

## F7 — `GDALWarp`

- **Location:** `ffi::gdal_warp`, called by `raster::warp` (every source
  read and every WMS composite).
- **Threads:** dataset-pool workers (source → MEM) and blocking-pool threads (tiles → MEM). Every dataset passed is owned by the calling thread for the whole call: the source is the pool worker's own dataset, and `dst` is held by `&mut` inside the same job. `GDALWarp` on *different* datasets from many threads is allowed by GDAL's re-entrancy rule; checked by `raster::tests::concurrent_warps_on_independent_datasets_agree` (16 threads × 50). GDAL's block cache, shared by all threads, is internally locked.
- **Arguments / memory passed:**
  - `pszDest`: NULL (we pass a destination dataset).
  - `hDstDS`: the handle of `dst: &mut Dataset`. The Rust `Dataset` owns it
    and closes it once, when dropped. Held mutably because GDAL writes into
    it.
  - `nSrcCount`, `pahSrcDS`: a Rust `Vec` of non-NULL handles of live
    datasets borrowed for the call (GDAL also rejects NULL entries,
    `gdalwarp_lib.cpp:1431-1435`). GDAL does not keep the array.
  - `psOptions`: the live options from F5.
  - `pbUsageError`: a Rust local.
- **Reference counting** (the property that matters for memory safety):
  for a caller-supplied `hDstDS`, GDAL takes one extra reference at entry
  (`GDALReferenceDataset`, `gdalwarp_lib.cpp:2478-2480`) and drops exactly
  that one on every exit — on error returns (e.g. `:3229-3232`) and in the
  final clean-up (`:3296-3297`). It never closes our dataset. Source
  datasets are referenced and released the same way (`:2940`, `:3040`,
  `:3110`). On success it returns `hDstDS` itself (`:3306`); kwms checks
  this.
- **Allocates / frees:** only GDAL-internal working memory (warp buffers,
  transformers, temporary VRTs), all freed before return. Results are
  written into `dst`'s bands.
- **Side effects:**
  - resets the CPL error state at entry (`gdalwarp_lib.cpp:1429`);
  - reads the source datasets (file or network I/O, through the block cache);
  - writes `dst`'s pixels, may set its nodata (`-dstnodata`), and flushes
    its cache (`:3290`);
  - on failure: CPL error state plus a stderr line.
- **Failure behaviour:** returns NULL (and/or sets `*pbUsageError`); kwms
  returns an internal error with the CPL message (F2–F4).
- **Verified by:** `raster::tests::failed_warps_never_release_or_leak_the_destination`.
  It forces 200 `GDALWarp` failures (zero pixel size, missing geotransform)
  and checks the destination's reference count stays 1 after each; then a
  successful warp into the same dataset also leaves it at 1. All other
  warp/composite tests and the end-to-end checks also exercise this path.

## F8 — `OSRGetDataAxisToSRSAxisMapping`

- **Location:** `ffi::data_axis_mapping`, used by `crs::native_axis_order`
  (startup only, config CRSes).
- **Threads:** startup only (catalog build, documents), before the listener is bound. The `SpatialRef` is a local of the calling function, never shared.
- **Arguments / memory passed:** the handle of a live `SpatialRef` (borrowed)
  and `pnCount` (a Rust local).
- **Returns:** `v.data()` of the SRS's internal `std::vector<int>`
  (`ogrspatialreference.cpp:12665-12675`). It is **owned by the SRS**: valid
  until the SRS is modified or destroyed. On invalid arguments GDAL returns
  NULL.
- **Allocates / frees:** nothing.
- **Side effects:** may take the SRS's optional lock
  (`ogrspatialreference.cpp:12648-12653`).
- **Failure behaviour:** NULL or a count of zero → error. kwms never falls
  back to a default axis order.

## F9 — `slice::from_raw_parts` on the axis mapping

- **Location:** `ffi::data_axis_mapping`.
- **Threads:** same thread and same call as F8; the borrowed `SpatialRef` is not reachable from any other thread.
- **Rust contract:** the pointer must be non-NULL and aligned, with `n`
  initialised elements and `n * size_of::<int>() <= isize::MAX`, and the
  memory must not be mutated for the slice's lifetime.
- **Why it holds:** NULL is rejected; the memory is a `std::vector<int>`'s
  buffer (aligned, initialised) and `n` is that vector's `size()`;
  the `SpatialRef` is borrowed immutably and is a local no other thread
  sees, so it can't change; the slice is copied into a `Vec` immediately.

## F10 — `OSRFreeSRSArray`

- **Location:** `Drop for ffi::SrsArray`.
- **Threads:** the thread that called F12, at the end of the same function.
- **Arguments / memory passed:** the non-NULL array returned by F12.
- **Frees:** releases each SRS in the NULL-terminated array (`OSRRelease`),
  then the array itself (`CPLFree`) (`ogrspatialreference.cpp:10193-10202`).
- **Exactly once:** only in `Drop`, which runs once on every path; the guard
  is created immediately after F12. The clones made in F15 are independent
  objects and are unaffected.

## F11 — `VSIFree` on the confidence array

- **Location:** `Drop for ffi::CplIntArray`.
- **Threads:** the thread that called F12, at the end of the same function.
- **Arguments / memory passed:** the non-NULL confidence array from F12.
- **Frees:** `free()` (`port/cpl_vsisimple.cpp:835`); `CPLFree` is `VSIFree`,
  which is what GDAL documents for this array.
- **Exactly once:** only in `Drop`; the guard is created immediately after F12.

## F12 — `OSRFindMatches`

- **Location:** `ffi::find_matches`, used by `crs::identify_crs` (startup
  only: catalog build and the source metadata probe).
- **Threads:** startup only, before the listener is bound: the catalog build and the source metadata probe (which runs on the source's dataset-pool worker). Uses the calling thread's PROJ context; the `SpatialRef` is local.
- **Arguments / memory passed:** the handle of a live `SpatialRef`; options
  NULL; `pnEntries` and `ppanMatchConfidence` are Rust locals, which GDAL
  sets to 0 / NULL first (`ogrspatialreference.cpp:10170-10181`).
- **Allocates:** if PROJ finds candidates, GDAL allocates (`:11800-11905`):
  - the result array with `CPLCalloc(n + 1)`, NULL-terminated, holding `n`
    newly allocated `OGRSpatialReference` objects;
  - the confidence array with `CPLMalloc(n + 1)`.

  Both functions abort on out-of-memory rather than return NULL. Ownership of
  both passes to us; they go into `SrsArray` and `CplIntArray` guards on the
  next statements (freed by F10/F11).
- **Frees:** GDAL frees its internal PROJ lists before returning.
- **Side effects:** PROJ database queries (disk reads of `proj.db`) on this
  thread's PROJ context; may take the SRS's optional lock; CPL error state.
- **Failure behaviour:** NULL is returned both for "no candidates" and for
  failure. kwms resets the CPL state first (F1) and treats NULL plus a
  `CE_Failure` as an error, otherwise as "no match".
- **Verified by:** `crs::gdal::tests::identify_crs_maps_esri_wkt_to_epsg` (an
  ESRI-WKT RD New resolves to `EPSG:28992`; an unparseable definition is an
  error) and `protocol::catalog` tests.

## F13 — `slice::from_raw_parts` on the match array

- **Location:** `ffi::find_matches`.
- **Threads:** same thread and same call as F12.
- **Why it holds:** the pointer is non-NULL (checked) and comes from
  `CPLCalloc`, so it is aligned; GDAL wrote a handle to each of the first
  `n` slots (`pahRet[i] = ToHandle(poSRS)` for `i < n`); the array lives
  until the `SrsArray` guard drops at the end of the function, after the last
  use of the slice. NULL entries are rejected before use.

## F14 — `slice::from_raw_parts` on the confidence array

- **Location:** `ffi::find_matches`.
- **Threads:** same thread and same call as F12.
- **Why it holds:** the pointer is non-NULL (checked; GDAL allocates it
  together with the match array) and comes from `CPLMalloc`, so it is
  aligned; the first `n` ints are written in the match loop (and rewritten
  on re-sort); it lives until the `CplIntArray` guard drops at the end of the
  function.

## F15 — `SpatialRef::from_c_obj` (clone a matched SRS)

- **Location:** `ffi::find_matches`.
- **Threads:** same thread as F12. The clone is owned by the caller and isn't shared afterwards.
- **Contract (gdal crate 0.17.1, `spatial_ref/srs.rs:53-61`):** the handle
  must be valid. The function asserts non-NULL — kwms checks first, so it
  never panics — and calls `OSRClone` (`ogrspatialreference.cpp:1471-1477`).
- **Allocates:** a new, independent `OGRSpatialReference` owned by the
  returned `SpatialRef`, released by that value's `Drop` (`OSRRelease`).
- **Frees:** nothing; the original stays owned by the array (F10).
- **Failure behaviour:** `OSRClone` returning NULL → error.

## F16 — `OSRSetPROJEnableNetwork`

- **Location:** `ffi::disable_proj_network`, called through
  `crs::disable_proj_network` at the start of `kwms serve` and in
  `AppState::from_parts`.
- **Threads:** startup thread, before any request can arrive. Safe from any
  thread: GDAL guards the global with `g_oSearchPathMutex`
  (`ogr_proj_p.cpp:479-495`, lock at 482).
- **Purpose:** forbid PROJ from downloading transformation grids
  (cdn.proj.org) during a coordinate transform, whatever `proj.ini` or
  `PROJ_NETWORK` say. kwms uses only locally installed grids.
- **Arguments / memory passed:** one `int` (`0`). No pointers.
- **Allocates:** nothing.
- **Frees:** nothing.
- **Side effects:** sets the process-wide `g_projNetworkEnabled` and bumps
  `g_projNetworkEnabledGenerationCounter` if the value changed. Each
  thread's PROJ context applies it (`proj_context_set_enable_network`) the
  next time GDAL fetches that context, under the same mutex
  (`ogr_proj_p.cpp:278, 299-307`). Every GDAL PROJ call goes through that
  fetch, so no thread can transform with a stale setting.
- **Failure behaviour:** returns `void`. With PROJ < 7 it raises a CPL
  error only when *enabling*; disabling is a no-op there (this build links
  PROJ 9.4.1). The setting is confirmed through F17 and startup fails if it
  did not apply.
- **Verified by:** `crs::gdal::tests::proj_network_access_is_disabled`.

## F17 — `OSRGetPROJEnableNetwork`

- **Location:** `ffi::proj_network_enabled`, called by
  `crs::disable_proj_network` right after F16.
- **Threads:** startup thread. Takes `g_oSearchPathMutex`
  (`ogr_proj_p.cpp:508-523`, lock at 511).
- **Purpose:** confirm that F16 took effect.
- **Arguments / memory passed:** none.
- **Allocates:** nothing once F16 has run. Had nothing set the value
  (`g_projNetworkEnabled < 0`), it would create this thread's PROJ context
  to ask PROJ (owned by GDAL, freed at thread exit); after F16 that branch
  can't run.
- **Frees:** nothing.
- **Side effects:** none after F16 (it only reads the global).
- **Failure behaviour:** returns an `int`; non-zero means enabled, which
  `crs::disable_proj_network` turns into a startup error.
- **Verified by:** `crs::gdal::tests::proj_network_access_is_disabled`.

## F18 — `VSISetPathSpecificOption`

- **Location:** `ffi::vsi_set_path_specific_option`, called by
  `source::gdal::credentials::register` from `AppState::new`.
- **Threads:** startup thread, before any source is opened and before the
  listener is bound. Safe from any thread: GDAL guards its map with
  `oMutexPathSpecificOptions` (`port/cpl_vsil.cpp:3836, 3889`).
- **Purpose:** give a source's credentials (from `secrets.yaml`) to GDAL's
  VSI handlers for one object-store path prefix, e.g. `/vsiaz/open-tiles/`.
  Unlike thread-local config options, these reach GDAL's own worker threads
  (GTI and VRT read tiles on a thread pool); unlike process-wide config
  options, they don't apply to other buckets.
- **Arguments / memory passed:** three `CString`s (prefix, key, value), owned
  by the caller and alive for the call. None is NULL (a NULL value would
  erase the option).
- **Allocates:** GDAL copies prefix, upper-cased key and value into the
  process-wide `std::map<std::string, std::map<std::string, std::string>>`
  `oMapPathSpecificOptions` (`cpl_vsil.cpp:3840-3841, 3886-3902`). Owned by
  GDAL for the life of the process; kwms never clears it.
- **Frees:** nothing.
- **Side effects:** from then on `VSIGetPathSpecificOption` returns the value
  for any path starting with the prefix (`cpl_vsil.cpp:3973-3990`; kwms
  always ends the prefix with `/`, so `/vsis3/foo/` never matches bucket
  `foobar`). The value sits in ordinary heap memory: it may reach swap or a
  core dump, like any secret in the process.
- **Hazard and mitigation:** `VSIGetPathSpecificOption` returns a pointer to
  the stored `std::string` *after* releasing the mutex (`:3976-3989`), so
  overwriting a value while a reader holds that pointer would be a
  use-after-free. `credentials::register` therefore writes each
  (prefix, key) at most once per process — repeating the same value is
  skipped, a different value is a startup error — and only before any
  dataset is opened.
- **Failure behaviour:** returns `void` and cannot fail short of an
  allocation failure (see "Global side effects"). NUL bytes are rejected on
  the Rust side before the call.
- **Verified by:**
  `source::gdal::credentials::tests::register_sets_gdal_path_specific_options_once`
  (read back through T3/T4: set under the prefix, absent for a sibling
  bucket) and, against real data, the 2026_orthoHR layer
  (`/vsiaz/` tiles read on GTI's worker threads).

## T1 — `GDALReferenceDataset` (tests only)

- **Location:** `ffi::dataset_ref_count`, compiled only under `cfg(test)`.
- **Threads:** test threads only; each test uses its own dataset.
- **Arguments:** the handle of a live dataset.
- **Effect:** increments the reference count and returns the new count
  (`gcore/gdaldataset.cpp:1570-1576`). No allocation.
- **Paired with T2** immediately after, so the count is restored.

## T2 — `GDALDereferenceDataset` (tests only)

- **Location:** `ffi::dataset_ref_count`, compiled only under `cfg(test)`.
- **Threads:** test threads only; each test uses its own dataset.
- **Effect:** decrements the reference count (`gdaldataset.cpp:1593-1614`).
  Unlike `GDALReleaseDataset` it **never deletes** the dataset, even if the
  count reached zero, so a mistake here cannot free memory still owned by
  the test.

## T3 — `VSIGetPathSpecificOption` (tests only)

- **Location:** `ffi::vsi_get_path_specific_option`, compiled only under
  `cfg(test)`.
- **Threads:** test threads only.
- **Arguments:** two caller-owned `CString`s (path, key) alive for the call;
  default NULL.
- **Effect:** under the mutex, finds the longest-sorted prefix of `path`
  holding `key`; otherwise falls back to `CPLGetConfigOption(key, NULL)`
  (`cpl_vsil.cpp:3973-3990`). Allocates nothing. Returns NULL or a pointer
  into GDAL's map or config options.

## T4 — `CStr::from_ptr` on the path-specific option

- **Location:** `ffi::vsi_get_path_specific_option`.
- **Why it holds:** the pointer is non-NULL (checked) and is a
  `std::string::c_str()` or a config option value, both NUL-terminated. It
  stays valid as long as nobody changes that option; F18 never changes a
  registered option, and it is copied into a `String` before any other
  GDAL call.

---

## Out of scope

- Unsafe code inside dependencies, including the safe wrappers of the `gdal`
  crate, `gdal-sys`, `tokio` and `hyper`. Those are trusted as maintained
  crates; an inventory can be produced with `cargo geiger`.
- Calls made through the `gdal` crate's safe API (`Dataset::open`,
  `RasterBand::read_as`, …). Their unsafe code belongs to the crate, not kwms.
