//! Accepted input bodies are located independently of queue ordering revisions.

use super::*;
use crate::flow_store::position::FlowBatch;
use crate::flow_store::{FlowFact, FlowStore};

impl FlowFact {
    fn accepted_input(self, id: Uuid) -> Option<AcceptedEnvelope> {
        let envelope = match self {
            Self::EnvelopeAccepted(envelope) => Some(*envelope),
            Self::Activated(record) => record.envelope(),
            Self::GoalUpdated(record) => record.input,
            Self::EventsObserved(record) => record.envelopes.into_iter().find(|item| item.id == id),
            Self::FlowWatchesExpired(record) => {
                record.envelopes.into_iter().find(|item| item.id == id)
            }
            Self::ProcessObserved(record) => record.envelope,
            Self::ProcessWatchesExpired(record) => {
                record.envelopes.into_iter().find(|item| item.id == id)
            }
            Self::TimersFired(record) => record.envelopes.into_iter().find(|item| item.id == id),
            Self::LegacyRecords(_)
            | Self::ImagesRetained(_)
            | Self::ExecutionEvents(_)
            | Self::Created(_)
            | Self::WorkspaceRequested(_)
            | Self::Transition(_)
            | Self::Controlled(_)
            | Self::TurnCompleted(_)
            | Self::InboxEdited(_)
            | Self::InputCommitted(_)
            | Self::ReplyWaiterRegistered(_)
            | Self::ReplyWaiterCancelled { .. }
            | Self::SendQueued(_)
            | Self::MessagePublished(_)
            | Self::ContextCheckpoint(_)
            | Self::HistoryHeadChanged(_)
            | Self::PlanningUpdated(_)
            | Self::NameUpdated(_)
            | Self::FlowSubscribed(_)
            | Self::FlowUnsubscribed(_)
            | Self::ProcessWatchRegistered(_)
            | Self::ProcessWatchCancelled(_)
            | Self::WatcherReplaced(_)
            | Self::TimerScheduled(_)
            | Self::TimerCancelled(_)
            | Self::TaskScopeCreated(_)
            | Self::TaskScopeCancelled(_)
            | Self::TaskScopeBound(_)
            | Self::InputScopeInvalidated(_)
            | Self::DeliveryConfirmed { .. }
            | Self::DeliveryFailed { .. } => None,
        };
        envelope.filter(|envelope| envelope.id == id)
    }
}

impl Inbox {
    pub(in crate::flow_store) fn read_entry(
        &self,
        id: Uuid,
        mut read: impl FnMut(&FlowPosition) -> io::Result<FlowBatch<FlowFact>>,
    ) -> io::Result<Option<InboxEntry>> {
        let Some(&index) = self.messages.get(&id) else {
            return Ok(None);
        };
        let entry = &self.entries[index];
        let batch = read(&entry.acceptance.receipt)?;
        let mut envelope = batch
            .facts
            .into_iter()
            .find_map(|fact| fact.accepted_input(id))
            .ok_or_else(|| invalid("accepted input is absent from its recorded batch"))?;
        if let Some(replacement) = self.replacements.get(&id) {
            let batch = read(replacement)?;
            let edit = batch
                .facts
                .into_iter()
                .find_map(|fact| match fact {
                    FlowFact::InboxEdited(edit) if edit.message == id => Some(*edit),
                    _ => None,
                })
                .ok_or_else(|| invalid("input replacement is absent from its recorded batch"))?;
            let InboxChange::Replace { content, delivery } = edit.change else {
                return Err(invalid(
                    "input body position does not contain a replacement",
                ));
            };
            envelope.request = envelope.request.replace_content(&content, delivery)?;
        }
        Ok(Some(InboxEntry {
            history_head: entry.envelope.history_head.clone(),
            envelope,
            acceptance: entry.acceptance.clone(),
            revision: entry.revision.clone(),
        }))
    }
}

impl FlowStore {
    /// Read the current input body from verified acceptance and replacement batches.
    pub fn inbox_entry(&mut self, id: Uuid) -> io::Result<Option<InboxEntry>> {
        self.inbox
            .read_entry(id, |receipt| self.journal.read_owned(receipt))
    }
}
