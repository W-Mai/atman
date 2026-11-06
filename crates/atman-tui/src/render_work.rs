//! Shared CPU preparation and completion notifications for document surfaces.

use std::sync::OnceLock;

use futures::executor::ThreadPool;
use tokio::sync::watch;

/// One cancellable CPU observation; unread results retire on the render workers.
pub(crate) struct Task<T: Send + 'static> {
    result: Option<std::sync::mpsc::Receiver<Result<T, String>>>,
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl<T: Send + 'static> Task<T> {
    pub(crate) fn spawn(work: impl FnOnce() -> T + Send + 'static) -> Self {
        Self::spawn_on(workers(), work)
    }

    pub(crate) fn spawn_on(pool: ThreadPool, work: impl FnOnce() -> T + Send + 'static) -> Self {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        };
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stop = cancelled.clone();
        pool.spawn_ok(async move {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let result = atman_runtime::capture_blocking_panic(work)
                .ok_or_else(|| "Preparation failed".to_owned());
            if !stop.load(Ordering::Relaxed) {
                let _ = sender.send(result);
                notify();
            }
        });
        Self {
            result: Some(receiver),
            cancelled,
        }
    }

    pub(crate) fn poll(&mut self) -> Option<Result<T, String>> {
        match self.result.as_ref()?.try_recv() {
            Ok(result) => {
                self.result = None;
                Some(result)
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => None,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                self.result = None;
                Some(Err("Preparation stopped".into()))
            }
        }
    }
}

impl<T: Send + 'static> Drop for Task<T> {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Relaxed);
        discard(self.result.take());
    }
}

pub(crate) fn workers() -> ThreadPool {
    static POOL: OnceLock<ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        ThreadPool::builder()
            .pool_size(2)
            .name_prefix("atman-render-")
            .create()
            .expect("create render workers")
    })
    .clone()
}

fn notifications() -> &'static watch::Sender<u64> {
    static CHANGES: OnceLock<watch::Sender<u64>> = OnceLock::new();
    CHANGES.get_or_init(|| watch::channel(0).0)
}

pub(crate) fn subscribe() -> watch::Receiver<u64> {
    notifications().subscribe()
}

pub(crate) fn notify() {
    notifications().send_modify(|revision| *revision = revision.wrapping_add(1));
}

pub(crate) fn discard(value: impl Send + 'static) {
    workers().spawn_ok(async move { drop(value) });
}
