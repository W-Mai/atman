//! Read-only work whose observation never retains runtime shutdown.

use std::sync::OnceLock;

use futures::executor::ThreadPool;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

fn workers() -> &'static ThreadPool {
    static POOL: OnceLock<ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        ThreadPool::builder()
            .pool_size(2)
            .name_prefix("atman-tui-read-")
            .create()
            .expect("create read workers")
    })
}

struct Observation<T: Send + 'static> {
    result: Option<oneshot::Receiver<Result<T, String>>>,
    cancellation: CancellationToken,
}

impl<T: Send + 'static> Drop for Observation<T> {
    fn drop(&mut self) {
        self.cancellation.cancel();
        let receiver = self.result.take();
        workers().spawn_ok(async move { drop(receiver) });
    }
}

pub(crate) async fn run<T: Send + 'static>(
    cancellation: &CancellationToken,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<Result<T, String>> {
    let cancelled = cancellation.child_token();
    let stop = cancelled.clone();
    let runtime = tokio::runtime::Handle::try_current().ok();
    let (sender, receiver) = oneshot::channel();
    let mut observation = Observation {
        result: Some(receiver),
        cancellation: cancelled.clone(),
    };
    workers().spawn_ok(async move {
        if stop.is_cancelled() {
            return;
        }
        let _context = runtime.as_ref().map(tokio::runtime::Handle::enter);
        let result = atman_runtime::capture_blocking_panic(work)
            .ok_or_else(|| "Background read failed".to_owned());
        let _ = sender.send(result);
    });
    tokio::select! {
        _ = cancelled.cancelled() => None,
        result = observation.result.as_mut().expect("read receiver") => Some(result.unwrap_or_else(|_| Err("Background read stopped".into()))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{Arc, mpsc},
        time::Duration,
    };

    struct DropThread(mpsc::Sender<std::thread::ThreadId>);
    impl Drop for DropThread {
        fn drop(&mut self) {
            let _ = self.0.send(std::thread::current().id());
        }
    }

    #[test]
    fn unread_read_results_retire_on_workers_when_observation_closes() {
        let (sender, receiver) = oneshot::channel();
        let (dropped, observed) = mpsc::channel();
        assert!(sender.send(Ok(DropThread(dropped))).is_ok());
        let cancellation = CancellationToken::new();
        let observation = Observation {
            result: Some(receiver),
            cancellation: cancellation.clone(),
        };
        let ui_thread = std::thread::current().id();
        drop(observation);
        assert!(cancellation.is_cancelled());
        assert_ne!(
            observed.recv_timeout(Duration::from_secs(2)).unwrap(),
            ui_thread
        );
    }

    #[test]
    fn blocked_read_retains_its_permit_but_does_not_hold_runtime_shutdown() {
        let (started, entered) = mpsc::channel();
        let (release, blocked) = mpsc::channel();
        let (stopped, shutdown) = mpsc::channel();
        let limit = Arc::new(tokio::sync::Semaphore::new(1));
        let worker_limit = limit.clone();
        let runtime_thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async move {
                let permit = worker_limit.acquire_owned().await.unwrap();
                let (active, observing) = oneshot::channel();
                tokio::spawn(async move {
                    run(&CancellationToken::new(), move || {
                        let _permit = permit;
                        started.send(()).unwrap();
                        active.send(()).unwrap();
                        blocked.recv().unwrap();
                    })
                    .await;
                });
                observing.await.unwrap();
            });
            drop(runtime);
            stopped.send(()).unwrap();
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let did_stop = shutdown.recv_timeout(Duration::from_secs(2));
        assert_eq!(
            limit.available_permits(),
            0,
            "a running read still owns its real slot"
        );
        release.send(()).unwrap();
        runtime_thread.join().unwrap();
        did_stop.expect("pure read retained Tokio shutdown until its gate was released");
    }
}
