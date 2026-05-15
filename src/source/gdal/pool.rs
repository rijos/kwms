//! Bounded, per-source pool of opened GDAL datasets.
//!
//! `gdal::Dataset` is `!Sync` (and only `Send` because the crate marks it so —
//! a dataset must be used from a single thread at a time). The pool here is
//! a **dedicated worker-thread pool**: one OS thread per slot, each owning
//! its own `Dataset` for its entire lifetime. Tokio callers submit a closure
//! to *any* idle worker via a shared crossbeam channel; the worker runs it on
//! its dataset and reports the result back over a `tokio::sync::oneshot`.
//!
//! This sidesteps the `!Send`-across-threads concern by ensuring each
//! `Dataset` is never moved off its owner thread once opened. See
//! `docs/src/source-abstraction.md`, "Dataset pooling".

use std::collections::HashMap;
use std::thread;

use anyhow::{anyhow, Context, Result};
use crossbeam_channel::{bounded, Sender};
use tokio::sync::oneshot;

use super::dataset::OpenedDataset;

/// A boxed unit of work that runs on a worker thread holding a `gdal::Dataset`.
type Job = Box<dyn FnOnce(&mut gdal::Dataset) + Send + 'static>;

pub struct DatasetPool {
    /// Submit jobs here; one of the worker threads will receive and run it.
    tx: Sender<Job>,
    /// Number of worker threads / dataset handles held by this pool.
    size: usize,
}

impl DatasetPool {
    /// Open `size` datasets at `uri` (each with `credentials` applied as
    /// scoped open options) and spawn one worker thread per dataset.
    ///
    /// `size` must be >= 1.
    pub fn new(uri: &str, credentials: HashMap<String, String>, size: usize) -> Result<Self> {
        if size == 0 {
            return Err(anyhow!("DatasetPool size must be at least 1"));
        }

        // A bounded channel of capacity `size` is enough to avoid unbounded
        // queueing in pathological burst scenarios; jobs will still queue
        // up to that depth before backpressure kicks in via `send`.
        let (tx, rx) = bounded::<Job>(size);

        for slot in 0..size {
            let opened = OpenedDataset::open(uri, &credentials)
                .with_context(|| format!("opening dataset slot {slot} for '{uri}'"))?;
            let rx = rx.clone();
            thread::Builder::new()
                .name(format!("gdal-pool[{slot}]"))
                .spawn(move || {
                    let mut owned = opened;
                    // Drain jobs until the channel is closed (all senders
                    // dropped). Each job borrows the dataset mutably.
                    while let Ok(job) = rx.recv() {
                        job(&mut owned.dataset);
                    }
                    // Ensure the dataset is dropped on this same thread.
                    drop(owned);
                })
                .with_context(|| format!("spawning gdal-pool worker thread {slot}"))?;
        }

        Ok(Self { tx, size })
    }

    /// Number of worker threads / dataset handles in this pool.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Run `f` on one of the pool's worker threads against its owned
    /// `gdal::Dataset`. The closure runs synchronously on the worker; this
    /// future resolves when the worker has finished and sent the result back.
    pub async fn with_dataset<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut gdal::Dataset) -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let (resp_tx, resp_rx) = oneshot::channel::<Result<R>>();

        // SAFETY (Send-safety): `gdal::Dataset` is marked `Send` by the
        // `gdal` crate, but the underlying GDAL docs require that a dataset
        // is only touched by one thread at a time. We enforce that by
        // *never* sharing the Dataset across threads: each worker thread
        // owns its Dataset for its entire lifetime, and only borrows it
        // mutably while running a single job. The `Job` closure we ship
        // through the channel is `Send + 'static`, but the `&mut Dataset`
        // it receives lives entirely inside the worker thread that runs it.
        let job: Job = Box::new(move |ds: &mut gdal::Dataset| {
            let out = f(ds);
            // Receiver may have been dropped (caller future cancelled); the
            // ignored Result is fine — we just discard the work product.
            let _ = resp_tx.send(out);
        });

        self.tx
            .send(job)
            .map_err(|_| anyhow!("DatasetPool worker channel closed"))?;

        resp_rx
            .await
            .map_err(|_| anyhow!("DatasetPool worker dropped response channel"))?
    }
}

impl Drop for DatasetPool {
    fn drop(&mut self) {
        // Dropping the `tx` end causes every worker's `rx.recv()` to return
        // `Err`, breaking its loop and letting it drop its Dataset on its
        // own thread. We don't `join` the threads: they exit promptly and
        // GDAL's resource cleanup is thread-safe.
    }
}
