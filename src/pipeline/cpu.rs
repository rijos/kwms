//! Bounded pool for CPU-bound, blocking GDAL work (encoding, WMS compositing).
//!
//! Jobs run on tokio's blocking thread pool via `spawn_blocking`, so they
//! never stall the async workers that serve other requests (health probes
//! included). A semaphore caps how many run at once: `spawn_blocking` alone
//! would happily start hundreds of multi-hundred-MB jobs in parallel.
//!
//! A job that can't get a slot within `queue_timeout` fails with
//! [`Busy`], which handlers turn into HTTP 429. The permit moves into the
//! blocking closure, so even when the request future is dropped (client gone,
//! request timeout) the slot stays taken until the GDAL work really finishes —
//! the limit holds for actual CPU use, not just for live requests.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::Semaphore;

use super::error::Busy;

#[derive(Clone)]
pub struct CpuPool {
    slots: Arc<Semaphore>,
    queue_timeout: Duration,
}

impl CpuPool {
    pub fn new(concurrency: usize, queue_timeout: Duration) -> Self {
        Self {
            slots: Arc::new(Semaphore::new(concurrency.max(1))),
            queue_timeout,
        }
    }

    /// Run `job` on the blocking pool once a slot is free.
    /// Run `job` on the blocking pool once a slot is free; its run time is
    /// recorded as `stage` (`kwms_stage_seconds`), the wait as `cpu_wait`.
    pub async fn run<F, R>(&self, stage: &'static str, job: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let waiting = std::time::Instant::now();
        let permit = tokio::time::timeout(self.queue_timeout, self.slots.clone().acquire_owned())
            .await
            .map_err(|_| Busy)?
            .context("render pool closed")?;
        super::stage::record("cpu_wait", waiting);
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            // Blocking threads are reused: start without another job's GDAL error.
            crate::ffi::cpl_error_reset();
            let running = std::time::Instant::now();
            let out = job();
            super::stage::record(stage, running);
            out
        })
        .await
        .context("render job panicked")?
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::pipeline::error::{classify, Disposition};

    #[tokio::test]
    async fn caps_concurrency_and_sheds_when_queue_times_out() {
        let pool = CpuPool::new(1, Duration::from_millis(50));
        let running = Arc::new(AtomicUsize::new(0));

        let r = running.clone();
        let slow = pool.run("test", move || {
            r.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(300));
            Ok(())
        });
        let blocked = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            pool.run("test", || Ok(())).await
        };
        let (slow, blocked) = tokio::join!(slow, blocked);
        slow.unwrap();
        let err = blocked.unwrap_err();
        assert_eq!(classify(&err), Disposition::Busy);
        assert_eq!(running.load(Ordering::SeqCst), 1);

        // Once the slot is free again, jobs run normally.
        assert_eq!(pool.run("test", || Ok(7)).await.unwrap(), 7);
    }
}
