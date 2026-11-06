//! Confession reads preserve their domain errors on shared observation workers.

use crate::error::RuntimeError;

pub(super) async fn capture<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, RuntimeError> + Send + 'static,
) -> Result<T, RuntimeError> {
    crate::read_work::capture(operation)
        .await
        .map_err(|_| RuntimeError::ToolFailed("confession reader panicked".into()))?
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
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (finished_tx, finished_rx) = mpsc::channel();
        let (exited_tx, exited_rx) = mpsc::channel();
        let owner = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap();
            runtime.block_on(async {
                tokio::spawn(capture(move || {
                    started_tx.send(()).unwrap();
                    let result = super::super::with_store_lock(&path, || Ok(()));
                    finished_tx.send(()).unwrap();
                    result
                }));
                started_rx.await.unwrap();
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
}
