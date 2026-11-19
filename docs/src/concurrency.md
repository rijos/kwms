# Concurrency

How kwms uses threads, how GDAL's threading rules are met, and why the
server is free of data races, starvation and deadlocks — with the tests that
check each claim. GDAL statements refer to GDAL 3.11.4
(`doc/source/user/multithreading.rst` and sources).

## Threads

| Thread(s) | Count | Runs | GDAL / PROJ? |
|---|---|---|---|
| **Startup** (inside `kwms serve`'s runtime, before the listener is bound) | 1 | PROJ network switched off, config load, `Catalog::build`, opening sources, coverage computation and the source-read budget check (`Renderer::new`), rendering all capabilities documents (`Documents::build`) | yes — before any request can arrive, so nothing can be starved |
| **Source probe** | 1 per source, transient | `GdalSource::open` probes metadata on a temporary thread + runtime | yes — startup only |
| **tokio workers** | CPU count | HTTP handling, `Kvp` parsing and validation, single-flight coordination, cache tier I/O, serving pre-rendered documents | **no** — no GDAL/PROJ call runs on a runtime worker during request handling |
| **Dataset-pool workers** (`gdal-pool[n]`) | 4 per source | source reads: `GDALWarp` from the worker's own dataset into a fresh MEM dataset | yes |
| **tokio blocking pool** (`spawn_blocking`) | up to 512 threads, *concurrency* capped at `server.render_concurrency` | encoding (MEM → `/vsimem/` → bytes), WMS compositing (`/vsimem/` tiles + `GDALWarp`) | yes |
| GDAL internal threads | 0 by default | GDAL creates worker threads only if `GDAL_NUM_THREADS` is set | — |

## GDAL's rules and how they are met

GDAL's public API is **re-entrant but not thread-safe**: functions may run
concurrently on *different* objects, never on the same object or on objects
tied by ownership (e.g. two bands of one dataset).

| Rule | How kwms meets it |
|---|---|
| A dataset (and its bands) is used by one thread at a time | Source datasets are owned for life by one pool worker each and only reached through jobs run on that thread. MEM datasets, `/vsimem/` tile datasets and encode datasets are created, used and dropped inside a single blocking job. Nothing shares a `Dataset` across threads. |
| `OGRSpatialReference` / transforms per thread | Created per call (`crs::gdal::spatial_ref`) and dropped before the call returns; never stored or shared. |
| Initialisation (`GDALAllRegister`) not concurrent | The `gdal` crate registers drivers through a `std::sync::Once`. `GDALDestroy`/`OSRCleanup` are never called. |
| Thread-local state | The CPL error context and the PROJ context are per thread. kwms resets and reads the error state inside one synchronous `ffi` call (never across an `.await`), so a message always belongs to the current call on the current thread. |
| Global state | The block cache and the `/vsimem/` file system are internally locked. `/vsimem/` names are unique per process (atomic counter), so threads never share a file. |
| No `fork()` mid-operation | kwms never forks. |

## Waits and locks

Everything a request can wait on, in the order it can be acquired:

| # | Wait | Bound | On exhaustion |
|---|---|---|---|
| 1 | request concurrency limit (`server.max_concurrent_requests`) | never waits (load shedding) | 429 |
| 2 | request time budget (`server.request_timeout`) | wraps 3–6 | 503 |
| 3 | single-flight follower wait (`Notify`) | until the leader finishes, fails or is cancelled | — |
| 4 | dataset-pool slot (FIFO semaphore) | `pool::QUEUE_TIMEOUT` (5 s) | 429 |
| 5 | render slot (FIFO semaphore) | `server.render_queue_timeout` (5 s) | 429 |
| 6 | job result (`oneshot`) | the job's run time | — |

Locks: the single-flight in-flight map (`std::sync::Mutex`) is held only for
one `HashMap` operation — never across an `.await`, never while taking
another lock, never while calling GDAL. There are no other mutexes on the
request path; the catalog, coverage map and documents are immutable after
startup.

## Deadlock freedom

- **No lock is held while waiting.** The only mutex is the in-flight map,
  held for a single map operation.
- **Slots are never nested.** A request holds at most one dataset-pool or
  render slot at a time, and releases it before taking the next: a tile is
  read (pool slot), *then* encoded (render slot); a WMS GetMap renders its
  tiles one after another, *then* composites — or, beyond the cached
  pyramid, reads its window (pool slot), *then* encodes (render slot). Jobs running in a slot never
  submit to a pool. With no request holding one resource while waiting for
  another, no wait cycle can form.
- **Single-flight can't strand a key.** The leader's registration is removed
  by a `Drop` guard, which also runs when the leader's future is cancelled
  (client gone, request timeout). Followers then wake and one becomes the
  next leader. A successful leader first publishes its result in the
  in-flight entry (a `OnceLock`), and woken followers return it — including
  results that are deliberately not cached (all-nodata tiles), which are
  therefore computed once per burst rather than once per follower.
- **Workers wait only for jobs.** A pool worker blocks only in `recv()`;
  replies go through a non-blocking `oneshot`.

## Starvation freedom

- **The async runtime never blocks.** No GDAL work runs on tokio workers
  (see *Threads*); submitting to the dataset pool waits asynchronously and
  hands over with a non-blocking `try_send` (a held permit guarantees
  channel space). Health probes and timers therefore stay responsive under
  any load.
- **Fair queues with a deadline.** Both slot semaphores are FIFO (tokio's
  `Semaphore` is fair), so waiters are served in order, and give up with
  429 after 5 s instead of queueing without bound.
- **No memory before a slot.** Image buffers are created inside the
  render-pool job (including the blank image for a BBOX outside the grid),
  so requests queued for a slot hold no image memory: peak image memory is
  bounded by `render_concurrency`, not by `max_concurrent_requests`.
- **No tile is expensive.** At startup every level of every grid is checked
  against the source-read budget (`source-abstraction.md`, "Overviews or
  bail"), so a client can't pick a tile that monopolises a pool worker for
  seconds.
- **Capacity can't leak.** A slot permit moves into the job and is released
  when the job finishes on its thread — even if the request was cancelled
  meanwhile — so blocking work in progress always counts against the limit.
- **Capacity can't shrink.** A panicking job (e.g. an integer overflow, which
  panics in every build) is caught; the pool worker reopens its dataset and
  continues, so a source never loses workers.
- **Shared limits are global, not per route.** The request concurrency limit
  is one semaphore shared by WMS, WMTS and TMS; health probes are exempt.

## Data-race freedom

- Rust code shares data only through `Arc`s of immutable values (config,
  catalog, coverage, documents), `Sync` caches (moka, opendal), atomics
  (request-id and `/vsimem/` counters, metrics) and the one mutex above.
- GDAL objects are never shared between threads (see the rules table); the
  `gdal` crate's `Send` for `Dataset` is only used to move a dataset into its
  owning pool worker once, at startup.

## Cancellation

When a request's future is dropped (client disconnect, request timeout):

| In progress | What happens |
|---|---|
| waiting for a slot | the wait is abandoned; no permit was taken |
| a pool / blocking job | the job runs to completion on its thread (GDAL calls can't be interrupted), then releases its permit; its result is discarded |
| single-flight leader | the guard unregisters the key and wakes followers; nothing was published, so they loop and one becomes leader |
| single-flight follower | stops waiting; nothing to clean up |

## Bugs found and fixed by this analysis

Each was reproduced by a test that failed before the fix.

| Bug | Class | Test |
|---|---|---|
| A cancelled single-flight leader left its key registered: the tile hung for every later request until restart | deadlock | `cache::layered::tests::cancelled_leader_does_not_strand_followers` |
| Followers created their wait future after releasing the lock, so a leader finishing in between was missed (lost wake-up) | race → hang | `single_flight_stress_never_hangs_and_computes_once` (300 rounds × 32 tasks) |
| Submitting to a full dataset pool did a blocking channel send on a tokio worker (634 ms stall measured) | starvation | `pool::tests::full_pool_does_not_block_the_async_runtime` |
| A panicking job killed its pool worker for good | starvation | `pool::tests::a_panicking_job_does_not_kill_the_worker` |
| Capabilities documents ran GDAL/PROJ transforms on tokio workers per request | starvation | now rendered at startup (`server::state::Documents`) |
| Tile coverage ran GDAL on tokio workers behind a poison-sensitive `Mutex`, and treated a failed transform as "covered" | starvation / silent fallback | now computed at startup (`Renderer::new`); failure stops startup |
| A coarse tile from a source without overviews read the whole source (12 s, 7 GB for one z0 tile of a 40000² GeoTIFF), holding a pool worker | starvation | `pipeline::read_budget` tests; startup refuses such sources |
| The blank image for an out-of-grid BBOX was allocated before waiting for a render slot (up to 512 × 2500² × bands × sample size) | memory exhaustion | buffer now built inside the render job |

## Verification

| Property | Test |
|---|---|
| No lost wake-up; one computation per key | `single_flight_stress_never_hangs_and_computes_once` |
| Leader failure hands over | `failed_leader_hands_over_to_a_follower` |
| Uncached results reach every waiter, computed once | `transient_results_reach_waiters_but_are_not_stored` |
| Cancelled leader doesn't strand followers | `cancelled_leader_does_not_strand_followers` |
| Pool never blocks the runtime | `full_pool_does_not_block_the_async_runtime` |
| Pool concurrency ≤ size (one job per dataset) | `concurrency_never_exceeds_pool_size` (64 jobs) |
| Saturated pool sheds with 429 | `saturated_pool_reports_busy` |
| Pool survives panics | `a_panicking_job_does_not_kill_the_worker` |
| Render pool caps concurrency and sheds | `pipeline::cpu::tests::caps_concurrency_and_sheds_when_queue_times_out` |
| Concurrent GDAL on independent datasets is correct | `raster::tests::concurrent_warps_on_independent_datasets_agree` (16 threads × 50) |
| GDAL error state is per thread | `raster::tests::cpl_error_state_is_isolated_per_thread` |
| Per-tile source reads are bounded | `read_budget::tests::*` (and the measured 45 ms worst tile with a full pyramid) |
| Overload → 429/503, health exempt | `server::tests::requests_over_the_concurrency_limit_are_shed`, `requests_over_the_time_budget_get_503` |

The concurrency tests were run 10 times in a row without a failure.

## Residual risks

- **Failed leaders are retried one at a time.** If a source keeps failing,
  queued followers each become leader in turn, so the last waiter can wait
  for several failed attempts (bounded by the request time budget).
- **Cancelled work still runs.** GDAL calls can't be interrupted; a
  cancelled request's in-progress job finishes (bounded by the slot limits).
- **`GDAL_NUM_THREADS`.** If set in the environment, GDAL spawns its own
  threads inside warps/decoding, multiplying CPU use beyond
  `render_concurrency`. Leave it unset (the default).
- **Memory fragmentation.** GDAL notes that multi-threaded raster I/O can
  inflate RSS with glibc's allocator; running with tcmalloc
  (`LD_PRELOAD=…/libtcmalloc_minimal.so.4`) mitigates it.
- **GDAL's default error handler** writes to stderr from any thread; lines
  from concurrent threads can interleave.
- **Not run under ThreadSanitizer.** GDAL isn't instrumented, so TSan would
  report false positives in GDAL; the stress tests above are used instead.
- **WMS fan-out.** Within the cached pyramid, one GetMap renders up to
  (WIDTH/256 + 2) × (HEIGHT/256 + 2) tiles (144 at 2500²) through the
  source's 4 pool workers, about 0.75 s of CPU at the maximum size. Beyond
  it, a GetMap is a single read holding one pool slot, capped at
  `read_budget::MAX_DIRECT_READ_PIXELS`. Bounded, but a few concurrent GetMaps can push
  WMTS/TMS requests for the same source into 429. Lower
  `server.max_request_pixels` if that matters.
- **`GDAL_CACHEMAX`** defaults to 5% of RAM and is process-wide; set it in
  the environment to bound GDAL's block cache explicitly.
- **Pool sizing** (4 datasets per source) and the pool queue timeout (5 s)
  are constants, not yet configuration.
