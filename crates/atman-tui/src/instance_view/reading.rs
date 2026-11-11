//! View-local branch reading positions tied to original conversation records.

use std::collections::HashMap;

use atman_runtime::{
    flow_store::source_capture::ConversationBranchId, projection::flow::FlowRecordId,
};

use crate::{
    flow_view::{DocumentIdentity, FlowViewModel},
    output::LayoutCache,
};

#[derive(Clone)]
struct Position {
    offset: u32,
    follow: bool,
    anchor: Option<(usize, FlowRecordId, u32)>,
}

pub(super) struct Captured {
    branch: ConversationBranchId,
    position: Position,
}

#[derive(Default)]
pub(crate) struct HistoryReading {
    saved: HashMap<ConversationBranchId, Position>,
    pending: Option<(DocumentIdentity, Position)>,
}

impl HistoryReading {
    pub(super) fn capture(
        &self,
        document: &FlowViewModel,
        cache: &LayoutCache,
        offset: u32,
        follow: bool,
    ) -> Option<Captured> {
        let branch = document.snapshot()?.history.as_ref()?.head().branch;
        let position = self
            .pending
            .as_ref()
            .filter(|(source, _)| *source == document.identity())
            .map(|(_, position)| position.clone())
            .unwrap_or_else(|| Position {
                offset,
                follow,
                anchor: (!follow)
                    .then(|| {
                        let (index, row) = cache.history_reading_position(document, offset)?;
                        Some((index, document.source_record(index)?, row))
                    })
                    .flatten(),
            });
        Some(Captured { branch, position })
    }

    pub(super) fn install(&mut self, captured: Captured, document: &FlowViewModel) -> (u32, bool) {
        let fallback = captured.position.clone();
        self.saved.insert(captured.branch, captured.position);
        let position = document
            .snapshot()
            .and_then(|snapshot| snapshot.history.as_ref())
            .and_then(|history| self.saved.remove(&history.head().branch))
            .unwrap_or(fallback);
        self.pending = (!position.follow
            && position
                .anchor
                .is_some_and(|(index, record, _)| document.source_record(index) == Some(record)))
        .then(|| (document.identity(), position.clone()));
        (position.offset, position.follow)
    }

    pub(crate) fn resolve(&mut self, document: &FlowViewModel, cache: &LayoutCache) -> Option<u32> {
        let (source, position) = self.pending.as_ref()?;
        let (index, record, local) = position.anchor?;
        if *source != document.identity() || document.source_record(index) != Some(record) {
            self.pending = None;
            return None;
        }
        let (offset, ready) = cache.history_reading_offset(document, index, local)?;
        if ready {
            self.pending = None;
        }
        Some(offset)
    }

    pub(crate) fn cancel(&mut self) {
        self.pending = None;
    }
}
