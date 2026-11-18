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
    pub async fn run<F, R>(&self, job: F) -> Result<R>
    where
        F: FnOnce() -> Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let permit = tokio::time::timeout(self.queue_timeout, self.slots.clone().acquire_owned())
            .await
            .map_err(|_| Busy)?
            .context("render pool closed")?;
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            job()
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
        let slow = pool.run(move || {
            r.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(300));
            Ok(())
        });
        let blocked = async {
            tokio::time::sleep(Duration::from_millis(20)).await;
            pool.run(|| Ok(())).await
        };
        let (slow, blocked) = tokio::join!(slow, blocked);
        slow.unwrap();
        let err = blocked.unwrap_err();
        assert_eq!(classify(&err), Disposition::Busy);
        assert_eq!(running.load(Ordering::SeqCst), 1);

        // Once the slot is free again, jobs run normally.
        assert_eq!(pool.run(|| Ok(7)).await.unwrap(), 7);
    }
}
