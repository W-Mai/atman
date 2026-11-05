//! Durable control lifetime is independent of its view and result observation.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::watch;

struct State {
    pending: AtomicUsize,
    changed: watch::Sender<()>,
}

#[derive(Clone)]
pub(crate) struct ControlWork(Arc<State>);

impl Default for ControlWork {
    fn default() -> Self {
        Self(Arc::new(State {
            pending: AtomicUsize::new(0),
            changed: watch::channel(()).0,
        }))
    }
}

impl ControlWork {
    pub fn is_waiting(&self) -> bool {
        self.0.pending.load(Ordering::Acquire) != 0
    }

    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.0.changed.subscribe()
    }

    pub(crate) fn begin(&self) -> Arc<Completion> {
        self.0.pending.fetch_add(1, Ordering::AcqRel);
        self.0.changed.send_replace(());
        Arc::new(Completion(self.clone()))
    }

    pub async fn settled(&self) {
        let mut changed = self.subscribe();
        while self.is_waiting() {
            if changed.changed().await.is_err() {
                break;
            }
        }
    }
}

pub(crate) struct Completion(ControlWork);

impl Drop for Completion {
    fn drop(&mut self) {
        self.0.0.pending.fetch_sub(1, Ordering::AcqRel);
        self.0.0.changed.send_replace(());
    }
}
