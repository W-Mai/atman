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
            Self::ProcessObserved(record) => {
                record.envelopes.into_iter().find(|item| item.id == id)
            }
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
            | Self::InboxRepairCancelled(_)
            | Self::InputCommitted(_)
            | Self::ReplyWaiterRegistered(_)
            | Self::ReplyWaiterCancelled { .. }
            | Self::SendQueued(_)
            | Self::MessagePublished(_)
            | Self::ContextCheckpoint(_)
            | Self::HistoryHeadChanged(_)
            | Self::HistoryOperation(_)
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
                    FlowFact::InboxRepairCancelled(_) => None,
                    _ => None,
                })
                .ok_or_else(|| invalid("input replacement is absent from its recorded batch"))?;
            let (InboxChange::Replace { content, delivery }
            | InboxChange::OperatorReplace {
                content, delivery, ..
            }) = edit.change
            else {
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

/// Bounded authored queue state captured at one input journal cursor.
#[derive(Debug)]
pub struct OperatorInboxPage {
    pub cursor: FlowPosition,
    pub head: HistoryHead,
    pub items: Vec<OperatorInboxItem>,
    pub next_turn_lanes: Vec<Vec<Uuid>>,
    pub offset: usize,
    pub total: usize,
    pub edits_available: bool,
    pub insertion_available: bool,
}

#[derive(Debug)]
pub struct OperatorInboxItem {
    pub id: Uuid,
    pub revision: FlowPosition,
    pub text: String,
    pub text_truncated: bool,
    pub image_count: usize,
    pub has_quote: bool,
    pub delivery: DeliveryMode,
    pub held: bool,
    pub separate_turn: bool,
}

pub const OPERATOR_INBOX_MAX_TEXT_BYTES: usize = 64 * 1024;
pub const OPERATOR_INBOX_MAX_MOVE: usize = 512;
const OPERATOR_INBOX_PAGE_SIZE: usize = 16;

impl Inbox {
    fn operator_page(
        &self,
        cursor: FlowPosition,
        state: &FlowInstance<AtmanUuid>,
        operator: super::super::local_operator::LocalOperator,
        offset: usize,
        mut read: impl FnMut(&FlowPosition) -> io::Result<FlowBatch<FlowFact>>,
    ) -> io::Result<OperatorInboxPage> {
        let mut next_turn_order = BTreeMap::<Uuid, Vec<Uuid>>::new();
        let mut visible = self
            .pending_entries()
            .filter(|entry| {
                let request = &entry.envelope.request;
                operator.owns(&request.sender) && request.is_editable_user_input()
            })
            .collect::<Vec<_>>();
        for item in &visible {
            let request = &item.envelope.request;
            if let SenderIdentity::User(sender) = &request.sender
                && request.delivery == DeliveryMode::NextTurn
                && item.envelope.history_head == self.head
            {
                let lane = next_turn_order.entry(*sender).or_default();
                if lane.len() <= OPERATOR_INBOX_MAX_MOVE {
                    lane.push(item.envelope.id);
                }
            }
        }
        visible.sort_by_key(|entry| entry.envelope.request.delivery != DeliveryMode::AtBoundary);
        let total = visible.len();
        let offset = (offset / OPERATOR_INBOX_PAGE_SIZE)
            .min(total.saturating_sub(1) / OPERATOR_INBOX_PAGE_SIZE)
            * OPERATOR_INBOX_PAGE_SIZE;
        let items = visible
            .into_iter()
            .skip(offset)
            .take(OPERATOR_INBOX_PAGE_SIZE)
            .map(|entry| {
                let id = entry.envelope.id;
                let request = self
                    .read_entry(id, &mut read)?
                    .ok_or_else(|| invalid("pending input disappeared"))?
                    .envelope
                    .request;
                let content = request
                    .editable_content()
                    .ok_or_else(|| invalid("input has no editable content"))?;
                let text = content.authored_text();
                let mut end = text.len().min(OPERATOR_INBOX_MAX_TEXT_BYTES);
                while !text.is_char_boundary(end) {
                    end -= 1;
                }
                Ok(OperatorInboxItem {
                    id,
                    revision: entry.revision.clone(),
                    text: text[..end].to_owned(),
                    text_truncated: end < text.len(),
                    image_count: content.images.len(),
                    has_quote: content
                        .presentation
                        .is_some_and(|presentation| presentation.quote.is_some()),
                    delivery: request.delivery,
                    held: entry.envelope.history_head != self.head,
                    separate_turn: matches!(entry.envelope.request.payload, InputKind::Invocation),
                })
            })
            .collect::<io::Result<Vec<_>>>()?;
        let edits_available = state.accepts_input() && !self.pending.has_reservation();
        let insertion_available = edits_available
            && matches!(state.state(),
            atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active))
                if !matches!(active.phase, atman_rt::TurnPhase::Finishing | atman_rt::TurnPhase::Cancelling));
        Ok(OperatorInboxPage {
            cursor,
            head: self.head.clone(),
            items,
            offset,
            total,
            edits_available,
            insertion_available,
            next_turn_lanes: next_turn_order
                .into_values()
                .filter(|lane| lane.len() <= OPERATOR_INBOX_MAX_MOVE)
                .collect(),
        })
    }
}

impl FlowStore {
    pub(crate) fn operator_inbox_page(
        &mut self,
        operator: super::super::local_operator::LocalOperator,
        offset: usize,
    ) -> io::Result<OperatorInboxPage> {
        self.inbox.operator_page(
            self.cursor.clone(),
            &self.state,
            operator,
            offset,
            |receipt| self.journal.read_unresolved(receipt),
        )
    }
}

impl crate::flow_store::SessionInspection {
    pub(crate) fn operator_inbox_page(
        &self,
        id: &atman_rt::FlowInstanceId<AtmanUuid>,
        operator: super::super::local_operator::LocalOperator,
        offset: usize,
    ) -> io::Result<OperatorInboxPage> {
        let mut read = crate::flow_store::read_registered_flow(
            &self.dir,
            &self.projection,
            id,
            |_, _| Ok(()),
        )?;
        let (_, state, inbox) = read
            .replay
            .projection
            .as_ref()
            .ok_or_else(|| invalid("flow creation is missing"))?;
        let cursor = read
            .replay
            .cursor
            .clone()
            .ok_or_else(|| invalid("flow journal has no cursor"))?;
        let mut page = inbox.operator_page(cursor, state, operator, offset, |receipt| {
            Ok(read.reader.read_batch(receipt.require_durable()?)?.into())
        })?;
        page.insertion_available = false;
        Ok(page)
    }
}
