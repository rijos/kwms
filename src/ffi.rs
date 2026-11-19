//! The only module allowed to contain `unsafe` code.
//!
//! Every raw GDAL call the safe `gdal` crate doesn't wrap lives here, behind
//! safe functions. The rest of the crate is compiled with
//! `#![deny(unsafe_code)]` (see `lib.rs`).
//!
//! Rules, enforced by lints and by `tests/unsafe_audit.rs`:
//!
//! * one unsafe operation per `unsafe` block
//!   (`clippy::multiple_unsafe_ops_per_block`);
//! * every block has a `// SAFETY:` comment
//!   (`clippy::undocumented_unsafe_blocks`) and an `// UNSAFE-AUDIT: <id>`
//!   tag;
//! * every tag has an entry in `docs/src/unsafe-audit.md` describing the
//!   call, the memory passed, what is allocated and freed and by whom, its
//!   side effects and failure behaviour — and every entry has a tag here.

use std::ffi::{c_int, CStr, CString};
use std::ptr::{self, NonNull};

use anyhow::{anyhow, bail, Context, Result};
use gdal::cpl::CslStringList;
use gdal::spatial_ref::SpatialRef;
use gdal::Dataset;

// ---------------------------------------------------------------------------
// CPL error state (thread-local)
// ---------------------------------------------------------------------------

/// Clear this thread's GDAL error state.
pub(crate) fn cpl_error_reset() {
    // UNSAFE-AUDIT: F1
    // SAFETY: no arguments; resets the calling thread's CPL error context.
    unsafe { gdal_sys::CPLErrorReset() }
}

/// Whether a GDAL error of severity `CE_Failure` or worse was raised on this
/// thread since the last [`cpl_error_reset`].
pub(crate) fn cpl_failed() -> bool {
    cpl_error_type() >= gdal_sys::CPLErr::CE_Failure
}

fn cpl_error_type() -> gdal_sys::CPLErr::Type {
    // UNSAFE-AUDIT: F2
    // SAFETY: no arguments; reads the calling thread's CPL error context.
    unsafe { gdal_sys::CPLGetLastErrorType() }
}

/// Longest GDAL message kept (they only go to server logs).
const MAX_CPL_MESSAGE_CHARS: usize = 512;

/// The message of the GDAL error raised on this thread since the last
/// [`cpl_error_reset`], copied into an owned `String` (at most
/// [`MAX_CPL_MESSAGE_CHARS`]); `None` if there is none.
///
/// Callers put it into internal errors, which `server::routes::error_reply`
/// logs and replaces with a generic message: it never reaches clients.
pub(crate) fn cpl_error_message() -> Option<String> {
    if cpl_error_type() == gdal_sys::CPLErr::CE_None {
        return None;
    }
    // UNSAFE-AUDIT: F3
    // SAFETY: no arguments; returns a pointer into the calling thread's CPL
    // error context (never NULL in GDAL 3.11; checked below anyway).
    let msg = unsafe { gdal_sys::CPLGetLastErrorMsg() };
    if msg.is_null() {
        return None;
    }
    // UNSAFE-AUDIT: F4
    // SAFETY: `msg` is non-NULL and points to CPL's NUL-terminated message
    // buffer for this thread, valid until the next CPL call on this thread;
    // it is copied into a `String` before any other CPL call.
    let text = unsafe { CStr::from_ptr(msg) };
    Some(text.to_string_lossy().chars().take(MAX_CPL_MESSAGE_CHARS).collect())
}

// ---------------------------------------------------------------------------
// GDALWarp
// ---------------------------------------------------------------------------

/// Owns a `GDALWarpAppOptions*`: non-NULL by construction, freed exactly
/// once when dropped.
pub(crate) struct WarpAppOptions(NonNull<gdal_sys::GDALWarpAppOptions>);

impl WarpAppOptions {
    /// Parse `gdalwarp`-style arguments. On error the CPL message says why.
    pub(crate) fn new(args: &CslStringList) -> Result<Self> {
        cpl_error_reset();
        // UNSAFE-AUDIT: F5
        // SAFETY: `args` is a valid NULL-terminated string list that outlives
        // the call; GDAL copies what it needs and keeps no pointer to it. The
        // second argument may be NULL.
        let raw = unsafe { gdal_sys::GDALWarpAppOptionsNew(args.as_ptr(), ptr::null_mut()) };
        NonNull::new(raw).map(Self).ok_or_else(|| {
            anyhow!(
                "GDALWarpAppOptionsNew rejected the options: {}",
                cpl_error_message().unwrap_or_else(|| "no GDAL message".into())
            )
        })
    }
}

impl Drop for WarpAppOptions {
    fn drop(&mut self) {
        // UNSAFE-AUDIT: F6
        // SAFETY: the pointer came from GDALWarpAppOptionsNew, is non-NULL,
        // and this is the only place it is freed (Drop runs once).
        unsafe { gdal_sys::GDALWarpAppOptionsFree(self.0.as_ptr()) }
    }
}

/// Run `GDALWarp` from `sources` into the existing dataset `dst`.
pub(crate) fn gdal_warp(sources: &[&Dataset], dst: &mut Dataset, options: &WarpAppOptions) -> Result<()> {
    let mut handles: Vec<gdal_sys::GDALDatasetH> = sources.iter().map(|d| d.c_dataset()).collect();
    if handles.iter().any(|h| h.is_null()) {
        bail!("internal: NULL source dataset handle");
    }
    let count = c_int::try_from(handles.len()).context("too many warp sources")?;
    let dst_handle = dst.c_dataset();
    if dst_handle.is_null() {
        bail!("internal: NULL destination dataset handle");
    }
    let mut usage_error: c_int = 0;
    // UNSAFE-AUDIT: F7
    // SAFETY: every handle in `handles` is a live, non-NULL dataset borrowed
    // for the whole call; `dst_handle` belongs to `dst`, held mutably, and
    // GDALWarp balances its own reference to it; `options` is live and
    // non-NULL; `usage_error` is a valid local.
    let out = unsafe {
        gdal_sys::GDALWarp(
            ptr::null(),
            dst_handle,
            count,
            handles.as_mut_ptr(),
            options.0.as_ptr(),
            &mut usage_error,
        )
    };
    if out.is_null() || usage_error != 0 {
        bail!(
            "GDALWarp failed: {}",
            cpl_error_message().unwrap_or_else(|| "no GDAL message".into())
        );
    }
    if out != dst_handle {
        bail!("internal: GDALWarp returned a dataset other than the destination");
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// OGRSpatialReference
// ---------------------------------------------------------------------------

/// The data-axis → CRS-axis mapping of `sr` (e.g. `[2, 1]`), copied out.
pub(crate) fn data_axis_mapping(sr: &SpatialRef) -> Result<Vec<c_int>> {
    let mut count: c_int = 0;
    // UNSAFE-AUDIT: F8
    // SAFETY: `sr` is a live SpatialRef (non-NULL handle); `count` is a valid
    // local. The returned pointer borrows `sr`'s internal vector.
    let ptr = unsafe { gdal_sys::OSRGetDataAxisToSRSAxisMapping(sr.to_c_hsrs(), &mut count) };
    let n = usize::try_from(count).context("negative axis count from GDAL")?;
    if ptr.is_null() || n == 0 {
        bail!("GDAL reported no axis mapping");
    }
    // UNSAFE-AUDIT: F9
    // SAFETY: `ptr` is non-NULL, aligned (from a std::vector<int>) and points
    // to `n` initialised ints owned by `sr`, which is borrowed (so not
    // mutated or dropped) until the copy below completes.
    let mapping = unsafe { std::slice::from_raw_parts(ptr, n) };
    Ok(mapping.to_vec())
}

/// Owns the NULL-terminated `OGRSpatialReferenceH*` from `OSRFindMatches`.
struct SrsArray(*mut gdal_sys::OGRSpatialReferenceH);

impl Drop for SrsArray {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // UNSAFE-AUDIT: F10
            // SAFETY: allocated by OSRFindMatches, non-NULL, freed only here.
            unsafe { gdal_sys::OSRFreeSRSArray(self.0) }
        }
    }
}

/// Owns the CPL-allocated confidence array from `OSRFindMatches`.
struct CplIntArray(*mut c_int);

impl Drop for CplIntArray {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // UNSAFE-AUDIT: F11
            // SAFETY: allocated with CPLMalloc by OSRFindMatches, non-NULL,
            // freed only here (CPLFree is VSIFree).
            unsafe { gdal_sys::VSIFree(self.0.cast()) }
        }
    }
}

/// All CRSes GDAL considers matches for `sr`, each cloned (owned) with its
/// confidence (0–100). An empty list means "no candidates"; `Err` means
/// GDAL failed.
pub(crate) fn find_matches(sr: &SpatialRef) -> Result<Vec<(SpatialRef, c_int)>> {
    let mut n: c_int = 0;
    let mut confidence_ptr: *mut c_int = ptr::null_mut();
    cpl_error_reset();
    // UNSAFE-AUDIT: F12
    // SAFETY: `sr` is a live SpatialRef; options may be NULL; `n` and
    // `confidence_ptr` are valid locals. Ownership of both returned
    // allocations moves into the guards on the next two statements.
    let raw = unsafe { gdal_sys::OSRFindMatches(sr.to_c_hsrs(), ptr::null_mut(), &mut n, &mut confidence_ptr) };
    let matches = SrsArray(raw);
    let confidences = CplIntArray(confidence_ptr);

    if matches.0.is_null() {
        if cpl_failed() {
            bail!(
                "GDAL failed to match the CRS: {}",
                cpl_error_message().unwrap_or_else(|| "no GDAL message".into())
            );
        }
        return Ok(Vec::new());
    }
    let n = usize::try_from(n).context("negative match count from GDAL")?;
    if confidences.0.is_null() {
        bail!("OSRFindMatches returned matches without confidences");
    }
    // UNSAFE-AUDIT: F13
    // SAFETY: `matches.0` is non-NULL, aligned and holds at least `n`
    // initialised handles (GDAL allocates n + 1, NULL-terminated); it lives
    // until `matches` drops at the end of this function.
    let handles = unsafe { std::slice::from_raw_parts(matches.0, n) };
    // UNSAFE-AUDIT: F14
    // SAFETY: `confidences.0` is non-NULL, aligned and holds at least `n`
    // initialised ints (GDAL allocates n + 1); it lives until `confidences`
    // drops at the end of this function.
    let scores = unsafe { std::slice::from_raw_parts(confidences.0, n) };

    let mut out = Vec::with_capacity(n);
    for (&handle, &score) in handles.iter().zip(scores) {
        if handle.is_null() {
            bail!("OSRFindMatches returned a NULL entry");
        }
        // UNSAFE-AUDIT: F15
        // SAFETY: `handle` is a non-NULL SRS owned by `matches`, alive for
        // the call; from_c_obj clones it (OSRClone), so the returned value
        // owns independent memory that outlives the array.
        let clone = unsafe { SpatialRef::from_c_obj(handle) }.context("cloning a matched CRS")?;
        out.push((clone, score));
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// PROJ networking
// ---------------------------------------------------------------------------

/// Turn PROJ's network access (on-demand downloads of transformation
/// grids from cdn.proj.org) off for every thread, overriding `proj.ini`
/// and the `PROJ_NETWORK` environment variable.
pub(crate) fn disable_proj_network() {
    // UNSAFE-AUDIT: F16
    // SAFETY: plain integer argument; GDAL stores it in a global under its
    // own mutex. No pointers are passed or returned.
    unsafe { gdal_sys::OSRSetPROJEnableNetwork(0) }
}

/// Whether PROJ network access is enabled, as GDAL will apply it.
pub(crate) fn proj_network_enabled() -> bool {
    // UNSAFE-AUDIT: F17
    // SAFETY: no arguments; reads a global under GDAL's own mutex (and, if
    // never set, queries this thread's PROJ context). Returns an integer.
    unsafe { gdal_sys::OSRGetPROJEnableNetwork() != 0 }
}

// ---------------------------------------------------------------------------
// VSI path-specific options (credentials)
// ---------------------------------------------------------------------------

/// Set GDAL option `key` to `value` for every VSI path starting with
/// `prefix` (`VSISetPathSpecificOption`). Process-wide, but consulted only
/// for paths under `prefix` — by every thread, including GDAL's own worker
/// threads (e.g. GTI's multi-threaded tile reads).
///
/// Must only be called at startup, before any dataset is opened under
/// `prefix`: GDAL hands out pointers into the stored values without holding
/// its lock, so overwriting a value while it is read would be a
/// use-after-free.
pub(crate) fn vsi_set_path_specific_option(prefix: &str, key: &str, value: &str) -> Result<()> {
    let prefix = CString::new(prefix).context("path prefix contains a NUL byte")?;
    let key = CString::new(key).context("option name contains a NUL byte")?;
    let value = CString::new(value).context("option value contains a NUL byte")?;
    // UNSAFE-AUDIT: F18
    // SAFETY: three valid NUL-terminated strings that outlive the call; GDAL
    // copies them into its own map under its own mutex and keeps no pointer
    // to them. None is NULL (NULL value would mean "erase").
    unsafe { gdal_sys::VSISetPathSpecificOption(prefix.as_ptr(), key.as_ptr(), value.as_ptr()) }
    Ok(())
}

// ---------------------------------------------------------------------------
// Test-only
// ---------------------------------------------------------------------------

/// The GDAL reference count of `ds` (1 = only our handle). Tests use it to
/// prove GDALWarp balances its references.
#[cfg(test)]
pub(crate) fn dataset_ref_count(ds: &Dataset) -> c_int {
    // UNSAFE-AUDIT: T1
    // SAFETY: `ds` is a live dataset; the reference taken here is dropped by
    // the very next block.
    let with_ours = unsafe { gdal_sys::GDALReferenceDataset(ds.c_dataset()) };
    // UNSAFE-AUDIT: T2
    // SAFETY: drops exactly the reference taken above; the count stays >= 1
    // because `ds` holds its own, so the dataset is never freed here.
    unsafe { gdal_sys::GDALDereferenceDataset(ds.c_dataset()) };
    with_ours.saturating_sub(1)
}

/// The value GDAL's VSI handlers would use for option `key` at `path`
/// (`VSIGetPathSpecificOption`), copied out. Tests use it to check what
/// [`vsi_set_path_specific_option`] registered.
#[cfg(test)]
pub(crate) fn vsi_get_path_specific_option(path: &str, key: &str) -> Result<Option<String>> {
    let path = CString::new(path).context("path contains a NUL byte")?;
    let key = CString::new(key).context("option name contains a NUL byte")?;
    // UNSAFE-AUDIT: T3
    // SAFETY: two valid NUL-terminated strings that outlive the call; the
    // default is NULL. The result is NULL or points into GDAL's option map
    // (or config options), valid while nobody changes that option — tests
    // never change a registered option.
    let value = unsafe { gdal_sys::VSIGetPathSpecificOption(path.as_ptr(), key.as_ptr(), ptr::null()) };
    if value.is_null() {
        return Ok(None);
    }
    // UNSAFE-AUDIT: T4
    // SAFETY: `value` is non-NULL and NUL-terminated (a `std::string`'s
    // `c_str()` or a config option value); copied before any other GDAL call.
    let text = unsafe { CStr::from_ptr(value) };
    Ok(Some(text.to_string_lossy().into_owned()))
}
