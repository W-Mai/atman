//! Shared CPU preparation and completion notifications for document surfaces.

use std::sync::OnceLock;

use futures::executor::ThreadPool;
use tokio::sync::watch;

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
