//! Read-only storage observation, independent of the host runtime's shutdown.

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, Ordering},
};

use futures::{executor::ThreadPool, future::RemoteHandle, task::SpawnExt};

use crate::error::RuntimeError;

fn workers() -> &'static ThreadPool {
    static WORKERS: OnceLock<ThreadPool> = OnceLock::new();
    WORKERS.get_or_init(|| {
        ThreadPool::builder()
            .pool_size(2)
            .name_prefix("atman-confession-read-")
            .create()
            .expect("create confession read workers")
    })
}

pub(super) async fn capture<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, RuntimeError> + Send + 'static,
) -> Result<T, RuntimeError> {
    ReadTask::start(workers(), operation).finish().await
}

struct ReadTask<T: Send + 'static> {
    work: Option<RemoteHandle<Option<Result<T, RuntimeError>>>>,
    cancelled: Arc<AtomicBool>,
    pool: ThreadPool,
}

impl<T: Send + 'static> ReadTask<T> {
    fn start(
        pool: &ThreadPool,
        operation: impl FnOnce() -> Result<T, RuntimeError> + Send + 'static,
    ) -> Self {
        let cancelled = Arc::new(AtomicBool::new(false));
        let obsolete = cancelled.clone();
        let work = pool
            .spawn_with_handle(async move {
                if obsolete.load(Ordering::Acquire) {
                    return None;
                }
                Some(
                    crate::panic_capture::blocking(operation).unwrap_or_else(|_| {
                        Err(RuntimeError::ToolFailed(
                            "confession reader panicked".into(),
                        ))
                    }),
                )
            })
            .expect("confession read workers remain open");
        Self {
            work: Some(work),
            cancelled,
            pool: pool.clone(),
        }
    }

    async fn finish(&mut self) -> Result<T, RuntimeError> {
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
    fn started_store_read_does_not_hold_runtime_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(dir.path().join(".confessions.lock"))
            .unwrap();
        fs2::FileExt::lock_exclusive(&lock).unwrap();
        let path = dir.path().to_owned();
        let (started_tx, started_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let (exited_tx, exited_rx) = mpsc::channel();
        let owner = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                let read = ReadTask::start(workers(), move || {
                    started_tx.send(()).unwrap();
                    let result = super::super::with_store_lock(&path, || Ok(()));
                    finished_tx.send(()).unwrap();
                    result
                });
                started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
                drop(read);
            });
            drop(runtime);
            exited_tx.send(()).unwrap();
        });
        let exited = exited_rx.recv_timeout(Duration::from_secs(2));
        assert!(finished_rx.try_recv().is_err());
        fs2::FileExt::unlock(&lock).unwrap();
        owner.join().unwrap();
        exited.expect("a started read must not retain Tokio shutdown");
        finished_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    #[test]
    fn cancelled_reads_skip_queued_work_and_retire_unread_results_on_workers() {
        let pool = ThreadPool::builder()
            .pool_size(1)
            .name_prefix("confession-read-contract-")
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
        let read = ReadTask::start(&pool, move || {
            observed.store(true, Ordering::SeqCst);
            Ok(())
        });
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
        let read = ReadTask::start(&pool, move || Ok(Retired(retired_tx)));
        futures::executor::block_on(pool.spawn_with_handle(async {}).unwrap());
        drop(read);
        assert!(
            retired_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap()
                .starts_with("confession-read-contract-")
        );
    }
}
