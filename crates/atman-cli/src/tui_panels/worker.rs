//! Poll host work away from the UI while retaining its original Tokio services.

use futures::{FutureExt, task::SpawnExt};
use std::{
    future::Future,
    pin::Pin,
    sync::OnceLock,
    task::{Context, Poll},
};

pub(crate) fn spawn_background<T: Send + 'static>(
    future: impl Future<Output = T> + Send + 'static,
) -> tokio::task::JoinHandle<T> {
    tokio::spawn(background(future))
}

struct InRuntime<F> {
    future: Pin<Box<F>>,
    runtime: tokio::runtime::Handle,
}

impl<F: Future> Future for InRuntime<F> {
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let _entered = this.runtime.enter();
        this.future.as_mut().poll(context)
    }
}

pub(crate) async fn background<T: Send + 'static>(
    future: impl Future<Output = T> + Send + 'static,
) -> T {
    static WORKERS: OnceLock<futures::executor::ThreadPool> = OnceLock::new();
    run(WORKERS.get_or_init(|| pool("atman-control-")), future).await
}

pub(crate) async fn urgent<T: Send + 'static>(
    future: impl Future<Output = T> + Send + 'static,
) -> T {
    static WORKERS: OnceLock<futures::executor::ThreadPool> = OnceLock::new();
    run(WORKERS.get_or_init(|| pool("atman-priority-")), future).await
}

pub(crate) async fn read_only<T: Send + 'static>(
    operation: impl FnOnce() -> T + Send + 'static,
) -> anyhow::Result<T> {
    static WORKERS: OnceLock<futures::executor::ThreadPool> = OnceLock::new();
    run(
        WORKERS.get_or_init(|| pool("atman-read-control-")),
        async move {
            atman_runtime::capture_blocking_panic(operation)
                .ok_or_else(|| anyhow::anyhow!("Background inspection failed"))
        },
    )
    .await
}

fn pool(name: &str) -> futures::executor::ThreadPool {
    futures::executor::ThreadPool::builder()
        .pool_size(2)
        .name_prefix(name)
        .create()
        .expect("create host control workers")
}

async fn run<T: Send + 'static>(
    pool: &futures::executor::ThreadPool,
    future: impl Future<Output = T> + Send + 'static,
) -> T {
    let future = InRuntime {
        future: Box::pin(future),
        runtime: tokio::runtime::Handle::current(),
    };
    let result = pool
        .spawn_with_handle(std::panic::AssertUnwindSafe(future).catch_unwind())
        .expect("host worker pool remains open")
        .await;
    match result {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readonly_inspection_keeps_its_actual_permit_without_retaining_runtime_shutdown() {
        use std::{
            sync::{Arc, mpsc},
            time::Duration,
        };
        let (started, entered) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let (stopped, shutdown) = mpsc::channel();
        let limit = Arc::new(tokio::sync::Semaphore::new(1));
        let source_limit = limit.clone();
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let permit = source_limit.acquire_owned().await.unwrap();
                let (active, observing) = tokio::sync::oneshot::channel();
                tokio::spawn(async move {
                    read_only(move || {
                        let _permit = permit;
                        started.send(()).unwrap();
                        active.send(()).unwrap();
                        blocked.recv().unwrap();
                    })
                    .await
                    .unwrap();
                });
                observing.await.unwrap();
            });
            drop(runtime);
            stopped.send(()).unwrap();
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let did_stop = shutdown.recv_timeout(Duration::from_secs(2));
        assert_eq!(limit.available_permits(), 0);
        release.send(()).unwrap();
        thread.join().unwrap();
        did_stop.expect("pure navigation read held runtime shutdown until its gate was released");
    }

    #[tokio::test]
    async fn pending_network_work_releases_threads_and_keeps_tokio_context_and_panic_isolation() {
        let pool = pool("atman-test-control-");
        let mut jobs = Vec::new();
        let mut releases = Vec::new();
        for _ in 0..2 {
            let (started, entered) = tokio::sync::oneshot::channel();
            let (release, wait) = tokio::sync::oneshot::channel();
            let workers = pool.clone();
            jobs.push(tokio::spawn(async move {
                run(&workers, async move {
                    started.send(()).unwrap();
                    let _ = wait.await;
                })
                .await
            }));
            entered.await.unwrap();
            releases.push(release);
        }
        let driver = std::thread::current().id();
        let thread = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            run(&pool, async {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                std::thread::current().id()
            }),
        )
        .await
        .unwrap();
        assert_ne!(driver, thread);
        for release in releases {
            let _ = release.send(());
        }
        for job in jobs {
            job.await.unwrap();
        }
        let workers = pool.clone();
        assert!(
            tokio::spawn(async move {
                run(&workers, async { panic!("controlled service failure") }).await
            })
            .await
            .unwrap_err()
            .is_panic()
        );
        assert_eq!(run(&pool, async { 42 }).await, 42);
    }

    #[tokio::test]
    async fn cancelled_observation_cannot_drop_an_in_progress_blocking_poll() {
        struct Receipt(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for Receipt {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().send(());
            }
        }
        let pool = pool("atman-test-control-");
        let (started, entered) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let (finished, mut observed) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            run(&pool, async move {
                let _receipt = Receipt(Some(finished));
                started.send(()).unwrap();
                let _ = wait.recv();
            })
            .await
        });
        entered.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(matches!(
            observed.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ));
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), observed)
            .await
            .unwrap()
            .unwrap();
    }
}
