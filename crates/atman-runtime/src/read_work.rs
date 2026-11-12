//! Read-only storage observation, independent of the host runtime's shutdown.

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

use futures::{executor::ThreadPool, future::RemoteHandle, task::SpawnExt};

fn workers() -> &'static ThreadPool {
    static WORKERS: OnceLock<ThreadPool> = OnceLock::new();
    WORKERS.get_or_init(|| {
        ThreadPool::builder()
            .pool_size(2)
            .name_prefix("atman-storage-read-")
            .create()
            .expect("create storage read workers")
    })
}

pub(crate) async fn capture<T: Send + 'static>(
    operation: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    ReadTask::start(workers(), operation).finish().await
}

pub(crate) fn retire(value: impl Send + 'static) {
    workers().spawn_ok(async move { drop(value) });
}

struct ReadTask<T: Send + 'static> {
    work: Option<RemoteHandle<Option<Result<T, String>>>>,
    cancelled: Arc<AtomicBool>,
    pool: ThreadPool,
}

impl<T: Send + 'static> ReadTask<T> {
    fn start(pool: &ThreadPool, operation: impl FnOnce() -> T + Send + 'static) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let obsolete = cancelled.clone();
        let runtime = tokio::runtime::Handle::try_current().ok();
        let work = pool
            .spawn_with_handle(async move {
                if obsolete.load(Ordering::Acquire) {
                    return None;
                }
                let _context = runtime.as_ref().map(tokio::runtime::Handle::enter);
                Some(
                    crate::panic_capture::blocking(operation)
                        .map_err(|_| "Background storage reader panicked".to_owned()),
                )
            })
            .expect("storage read workers remain open");
        Self {
            work: Some(work),
            cancelled,
            pool: pool.clone(),
        }
    }

    async fn finish(&mut self) -> Result<T, String> {
        let result = self
            .work
            .as_mut()
            .expect("read has one consumer")
            .await
            .expect("observed read is not cancelled");
        self.work = None;
        result
    }
}

impl<T: Send + 'static> Drop for ReadTask<T> {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(work) = self.work.take() {
            // Queued closures and unread results are also retired away from callers.
            self.pool.spawn_ok(async move { drop(work) });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[test]
    fn returned_storage_owners_are_retired_off_the_caller_thread() {
        struct Retired(mpsc::Sender<String>);
        impl Drop for Retired {
            fn drop(&mut self) {
                let _ = self
                    .0
                    .send(std::thread::current().name().unwrap_or("").into());
            }
        }
        let (sender, receiver) = mpsc::channel();
        retire(Retired(sender));
        assert!(
            receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .starts_with("atman-storage-read-")
        );
    }

    #[test]
    fn cancelled_reads_skip_queued_work_and_retire_unread_results_on_workers() {
        let pool = ThreadPool::builder()
            .pool_size(1)
            .name_prefix("storage-read-contract-")
            .create()
            .unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        pool.spawn_ok(async move {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let called = Arc::new(AtomicBool::new(false));
        let observed = called.clone();
        let read = ReadTask::start(&pool, move || observed.store(true, Ordering::SeqCst));
        drop(read);
        release_tx.send(()).unwrap();
        futures::executor::block_on(pool.spawn_with_handle(async {}).unwrap());
        assert!(!called.load(Ordering::SeqCst));

        struct Retired(mpsc::Sender<String>);
        impl Drop for Retired {
            fn drop(&mut self) {
                self.0
                    .send(std::thread::current().name().unwrap_or("").into())
                    .unwrap();
            }
        }
        let (retired_tx, retired_rx) = mpsc::channel();
        let read = ReadTask::start(&pool, move || Retired(retired_tx));
        futures::executor::block_on(pool.spawn_with_handle(async {}).unwrap());
        drop(read);
        assert!(
            retired_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .starts_with("storage-read-contract-")
        );
    }
}
