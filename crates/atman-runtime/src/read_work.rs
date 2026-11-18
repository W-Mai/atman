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

fn coordination_workers() -> &'static ThreadPool {
    static WORKERS: OnceLock<ThreadPool> = OnceLock::new();
    WORKERS.get_or_init(|| {
        ThreadPool::builder()
            .pool_size(2)
            .name_prefix("atman-authority-")
            .create()
            .expect("create authority coordination workers")
    })
}

/// Short nonblocking OS-lock attempts must not wait behind journal scans or asset copies.
pub(crate) async fn coordinate<T: Send + 'static>(
    operation: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    ReadTask::start(coordination_workers(), operation)
        .finish()
        .await
}

pub(crate) fn retire_coordination(value: impl Send + 'static) {
    coordination_workers().spawn_ok(async move { drop(value) });
}

pub(crate) async fn capture<T: Send + 'static>(
    operation: impl FnOnce() -> T + Send + 'static,
) -> Result<T, String> {
    ReadTask::start(workers(), operation).finish().await
}

pub(crate) fn retire(value: impl Send + 'static) {
    workers().spawn_ok(async move { drop(value) });
}

#[cfg(test)]
pub(crate) async fn storage_saturation_gate() -> tokio::sync::MutexGuard<'static, ()> {
    static SATURATION: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
    SATURATION.lock().await
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

    #[tokio::test]
    async fn native_authority_acquire_and_release_do_not_wait_for_storage_readers() {
        use crate::peer_authority::{AuthorityKey, AuthorityMode, AuthorityPlan, PeerAuthority};
        let _saturation = storage_saturation_gate().await;
        let mut readers = Vec::new();
        struct ReleaseReaders(Vec<mpsc::Sender<()>>);
        impl Drop for ReleaseReaders {
            fn drop(&mut self) {
                for release in self.0.drain(..) {
                    let _ = release.send(());
                }
            }
        }
        let mut release = ReleaseReaders(Vec::new());
        for _ in 0..2 {
            let (entered, ready) = tokio::sync::oneshot::channel();
            let (finish, blocked) = mpsc::channel();
            release.0.push(finish);
            readers.push(ReadTask::start(workers(), move || {
                let _ = entered.send(());
                blocked.recv_timeout(Duration::from_secs(10)).unwrap();
            }));
            tokio::time::timeout(Duration::from_secs(5), ready)
                .await
                .unwrap()
                .unwrap();
        }
        let data = tempfile::tempdir().unwrap();
        let authority =
            PeerAuthority::new(data.path().canonicalize().unwrap(), uuid::Uuid::now_v7()).unwrap();
        let plan = AuthorityPlan::new([(
            AuthorityKey::Session(uuid::Uuid::now_v7()),
            AuthorityMode::Exclusive,
        )])
        .unwrap();
        let started = std::time::Instant::now();
        let measured = tokio::time::timeout(Duration::from_secs(2), async {
            for index in 0..32 {
                let guard = authority
                    .acquire(
                        plan.clone(),
                        tokio_util::sync::CancellationToken::new(),
                        std::time::Instant::now() + Duration::from_secs(1),
                    )
                    .await?;
                if index % 2 == 0 {
                    guard.release().await?;
                } else {
                    drop(guard);
                }
            }
            Ok::<_, std::io::Error>(started.elapsed())
        })
        .await;
        drop(release);
        for reader in &mut readers {
            reader.finish().await.unwrap();
        }
        let elapsed = measured.unwrap().unwrap();
        eprintln!(
            "native authority acquire/release: n=32 total_us={} mean_us={:.1}",
            elapsed.as_micros(),
            elapsed.as_secs_f64() * 1_000_000.0 / 32.0
        );
    }

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
