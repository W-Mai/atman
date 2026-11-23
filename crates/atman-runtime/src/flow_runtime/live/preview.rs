//! Read-only compact display observations from existing Flow workers.

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::{FlowWorker, HistoryHead, LiveOutput, PreviewSnapshot, State};

pub struct CompactPreview {
    pub snapshot: PreviewSnapshot,
    pub head: HistoryHead,
    pub host_generation: Uuid,
    pub worker_incarnation: Uuid,
    pub revision: u64,
}

/// Retains the bounded display state, but does not hold the Session, journal, or execution owner.
pub struct CompactPreviewReader {
    host_generation: Uuid,
    worker_incarnation: Uuid,
    lifetime: CancellationToken,
    changed: watch::Receiver<Option<CompactUpdate>>,
}

pub(super) struct CompactUpdate {
    snapshot: PreviewSnapshot,
    head: HistoryHead,
    revision: u64,
}

impl FlowWorker {
    /// The caller must authorize this address and resolve an already loaded worker.
    pub fn observe_compact_preview(&self) -> Option<CompactPreviewReader> {
        let host = self.host.as_ref()?.upgrade().ok()?;
        if self.is_stopped() || self.is_transient() || self.resources.lifetime.is_cancelled() {
            return None;
        }
        let source = &self.live_output;
        let state = source.state.lock().unwrap();
        if source.preview_changed.receiver_count() == 0 {
            source.publish_preview(&state);
        }
        Some(CompactPreviewReader {
            host_generation: host.generation(),
            worker_incarnation: source.incarnation,
            lifetime: self.resources.lifetime.clone(),
            changed: source.preview_changed.subscribe(),
        })
    }
}

impl LiveOutput {
    pub(super) fn compact_snapshot(&self, state: &State) -> PreviewSnapshot {
        PreviewSnapshot {
            flow: self.flow.clone(),
            text: state.preview.clone(),
            active: state.preview_lease.clone(),
            completion: state.completion.clone(),
        }
    }

    // Called while the state lock still couples the revision to its compact fields.
    pub(super) fn notify_preview(&self, state: &mut State) {
        state.preview_revision = state.preview_revision.wrapping_add(1);
        if self.preview_changed.receiver_count() != 0 {
            self.publish_preview(state);
        }
    }

    fn publish_preview(&self, state: &State) {
        self.preview_changed
            .send_replace(state.history_head.clone().map(|head| CompactUpdate {
                snapshot: self.compact_snapshot(state),
                head,
                revision: state.preview_revision,
            }));
    }
}

impl CompactPreviewReader {
    pub fn snapshot(&mut self) -> Option<CompactPreview> {
        if self.lifetime.is_cancelled() {
            return None;
        }
        let value = self.changed.borrow_and_update();
        let update = value.as_ref()?;
        Some(CompactPreview {
            snapshot: update.snapshot.clone(),
            head: update.head.clone(),
            host_generation: self.host_generation,
            worker_incarnation: self.worker_incarnation,
            revision: update.revision,
        })
    }

    /// Returns false when this worker has retired. A true return requires another snapshot.
    pub async fn changed(&mut self) -> bool {
        tokio::select! {
            biased;
            _ = self.lifetime.cancelled() => false,
            change = self.changed.changed() => change.is_ok(),
        }
    }
}
