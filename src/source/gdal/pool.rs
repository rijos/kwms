//! Bounded, per-source pool of opened GDAL datasets.
//!
//! GDAL rule (docs/src/concurrency.md): a dataset — and the bands it owns —
//! must never be used by two threads at once. The pool enforces it by
//! construction: one OS thread per slot, each owning its own `Dataset` for
//! its whole life; jobs are closures run *on* that thread with `&mut` access.
//!
//! Concurrency properties:
//! * **Admission never blocks a runtime thread.** Callers first take a slot
//!   permit from an async, FIFO-fair `tokio::sync::Semaphore` (one permit per
//!   worker), waiting at most [`QUEUE_TIMEOUT`] before failing with [`Busy`]
//!   (HTTP 429). Holding a permit guarantees the bounded job channel has
//!   room, so the hand-off is a non-blocking `try_send`.
//! * **Capacity is exact under cancellation.** The permit moves into the job
//!   and is released only when the job has run on the worker, even if the
//!   caller's future was dropped meanwhile.
//! * **A panicking job doesn't kill the worker.** Each job runs under
//!   `catch_unwind`; after a panic the worker drops its dataset and reopens
//!   it, so no state touched by the failed job is reused.
//! * **No deadlock:** a worker never waits for anything but its next job,
//!   and jobs never submit to a pool themselves.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{bounded, Sender, TrySendError};
use tokio::sync::{oneshot, OwnedSemaphorePermit, Semaphore};

use super::dataset::OpenedDataset;
use crate::pipeline::error::Busy;

/// Longest a caller waits for a free dataset slot before getting [`Busy`].
pub const QUEUE_TIMEOUT: Duration = Duration::from_secs(5);

/// A unit of work run on a worker thread against its dataset. It carries the
/// slot permit, released when the job (and the closure inside) is dropped.
type Job = Box<dyn FnOnce(&mut gdal::Dataset) + Send + 'static>;

pub struct DatasetPool {
    tx: Sender<Job>,
    slots: Arc<Semaphore>,
    size: usize,
}

impl DatasetPool {
    /// Open `size` datasets at `uri` (each with `open_options`) and spawn
    /// one worker thread per dataset.
    pub fn new(uri: &str, open_options: &[String], size: usize) -> Result<Self> {
        if size == 0 {
            return Err(anyhow!("DatasetPool size must be at least 1"));
        }
        // Capacity == number of permits: a permit holder can always enqueue.
        let (tx, rx) = bounded::<Job>(size);
        for slot in 0..size {
            let opened = OpenedDataset::open(uri, open_options)
                .with_context(|| format!("opening dataset slot {slot} for '{uri}'"))?;
            let rx = rx.clone();
            let (uri, open_options) = (uri.to_string(), open_options.to_vec());
            thread::Builder::new()
                .name(format!("gdal-pool[{slot}]"))
                .spawn(move || worker(slot, opened, rx, &uri, &open_options))
                .with_context(|| format!("spawning gdal-pool worker thread {slot}"))?;
        }
        Ok(Self {
            tx,
            slots: Arc::new(Semaphore::new(size)),
            size,
        })
    }

    /// Number of worker threads / dataset handles in this pool.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Run `f` on one of the pool's worker threads against its own dataset.
    pub async fn with_dataset<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut gdal::Dataset) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let waiting = std::time::Instant::now();
        let permit: OwnedSemaphorePermit = tokio::time::timeout(QUEUE_TIMEOUT, self.slots.clone().acquire_owned())
            .await
            .map_err(|_| Busy)?
            .context("dataset pool closed")?;
        crate::pipeline::stage::record("gdal_wait", waiting);
        let (resp_tx, resp_rx) = oneshot::channel::<Result<R>>();
        let job: Job = Box::new(move |ds: &mut gdal::Dataset| {
            let _permit = permit; // released when this job finishes or unwinds
            let running = std::time::Instant::now();
            let result = f(ds);
            crate::pipeline::stage::record("gdal_run", running);
            // The caller may be gone (cancelled); the result is then dropped.
            let _ = resp_tx.send(result);
        });
        match self.tx.try_send(job) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                return Err(anyhow!("internal: dataset pool channel full while holding a slot"))
            }
            Err(TrySendError::Disconnected(_)) => return Err(anyhow!("dataset pool has no workers")),
        }
        resp_rx
            .await
            .map_err(|_| anyhow!("dataset pool job failed (worker panicked or was shut down)"))?
    }
}

/// Worker loop: run jobs on this thread's dataset until the pool is dropped.
fn worker(
    slot: usize,
    mut dataset: OpenedDataset,
    rx: crossbeam_channel::Receiver<Job>,
    uri: &str,
    open_options: &[String],
) {
    while let Ok(job) = rx.recv() {
        if catch_unwind(AssertUnwindSafe(|| job(&mut dataset.dataset))).is_ok() {
            continue;
        }
        tracing::error!(slot, uri, "dataset pool job panicked; reopening the dataset");
        // Don't reuse a dataset a panicking job may have left mid-operation.
        drop(dataset);
        match OpenedDataset::open(uri, open_options) {
            Ok(fresh) => dataset = fresh,
            Err(e) => {
                tracing::error!(slot, uri, error = %format_args!("{e:#}"), "cannot reopen dataset; worker exits");
                return;
            }
        }
    }
    // Channel closed (pool dropped): `dataset` is dropped here, on its own thread.
}

impl Drop for DatasetPool {
    fn drop(&mut self) {
        // Dropping `tx` closes the channel; each worker finishes its current
        // job, leaves its loop and drops its dataset on its own thread.
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;

    fn tiny_tif() -> tempfile::NamedTempFile {
        let tmp = tempfile::Builder::new().suffix(".tif").tempfile().unwrap();
        let driver = gdal::DriverManager::get_driver_by_name("GTiff").unwrap();
        driver.create_with_band_type::<u8, _>(tmp.path(), 4, 4, 1).unwrap();
        tmp
    }

    /// Submitting to a busy pool must wait *asynchronously*: a blocked
    /// runtime thread would stall every other task on it (health checks,
    /// timers, other requests).
    #[test]
    fn full_pool_does_not_block_the_async_runtime() {
        let tif = tiny_tif();
        let pool = Arc::new(DatasetPool::new(tif.path().to_str().unwrap(), &[], 1).unwrap());
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let worst_tick = rt.block_on(async {
            for _ in 0..4 {
                let p = pool.clone();
                tokio::spawn(async move {
                    let _ = p
                        .with_dataset(|_| {
                            std::thread::sleep(Duration::from_secs(1));
                            Ok(())
                        })
                        .await;
                });
            }
            // A 10 ms ticker on the same (only) runtime thread.
            let mut worst = Duration::ZERO;
            for _ in 0..20 {
                let t = Instant::now();
                tokio::time::sleep(Duration::from_millis(10)).await;
                worst = worst.max(t.elapsed());
            }
            worst
        });
        // A blocking hand-off stalls for at least one job (1 s); allow generous
        // scheduling noise below that.
        assert!(
            worst_tick < Duration::from_millis(250),
            "runtime thread was blocked for {worst_tick:?}"
        );
    }

    /// A job that panics must not permanently remove a worker.
    #[test]
    fn a_panicking_job_does_not_kill_the_worker() {
        let tif = tiny_tif();
        let pool = DatasetPool::new(tif.path().to_str().unwrap(), &[], 1).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let first = pool.with_dataset(|_| -> Result<()> { panic!("job bug") }).await;
            assert!(first.is_err());
            let second = tokio::time::timeout(Duration::from_secs(2), pool.with_dataset(|ds| Ok(ds.raster_size())))
                .await
                .expect("pool has no live worker after a panic");
            assert_eq!(second.unwrap(), (4, 4));
        });
    }

    /// Many concurrent jobs: never more than `size` run at once (one per
    /// worker/dataset) and all complete.
    #[test]
    fn concurrency_never_exceeds_pool_size() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let tif = tiny_tif();
        let pool = Arc::new(DatasetPool::new(tif.path().to_str().unwrap(), &[], 4).unwrap());
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let mut tasks = Vec::new();
            for _ in 0..64 {
                let (pool, running, peak) = (pool.clone(), running.clone(), peak.clone());
                tasks.push(tokio::spawn(async move {
                    pool.with_dataset(move |ds| {
                        let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(now, Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(5));
                        let size = ds.raster_size();
                        running.fetch_sub(1, Ordering::SeqCst);
                        Ok(size)
                    })
                    .await
                }));
            }
            for t in tasks {
                assert_eq!(t.await.unwrap().unwrap(), (4, 4));
            }
        });
        assert!(
            peak.load(Ordering::SeqCst) <= 4,
            "peak {} > pool size",
            peak.load(Ordering::SeqCst)
        );
    }

    /// A saturated pool sheds load with `Busy` after the queue timeout
    /// instead of queueing without bound.
    #[test]
    fn saturated_pool_reports_busy() {
        use crate::pipeline::error::{classify, Disposition};
        let tif = tiny_tif();
        let pool = Arc::new(DatasetPool::new(tif.path().to_str().unwrap(), &[], 1).unwrap());
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let p = pool.clone();
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let hog = tokio::spawn(async move {
                p.with_dataset(move |_| {
                    let _ = started_tx.send(()); // the only slot is now taken
                    std::thread::sleep(QUEUE_TIMEOUT + Duration::from_secs(1));
                    Ok(())
                })
                .await
            });
            started_rx.await.unwrap();
            let err = pool.with_dataset(|_| Ok(())).await.unwrap_err();
            assert_eq!(classify(&err), Disposition::Busy);
            hog.await.unwrap().unwrap();
        });
    }
}
