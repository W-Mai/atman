//! Instance-scoped transcript projection from committed journal batches.

use std::{collections::BTreeMap, io, ops::Range};

use serde::{Deserialize, Serialize};

use super::flow_workflow::FlowWorkflows;
pub use super::flow_workflow::{FlowTurnEnd, FlowTurnProjection};
mod archive;
pub use archive::{ArchiveTranscript, LegacyFlowView, LegacyHistory};

use crate::{
    flow_store::position::{FlowBatch, FlowPosition},
    flow_store::{
        FlowRef,
        completion::Completion,
        context::ContextCheckpoint,
        history::LegacyHistoryRecord,
        inbox::{AcceptedEnvelope, CommittedInput, InboxPayload, InputContent, SenderIdentity},
        publication::MessagePublication,
        trace::ExecutionEvent,
        view::{AcceptedInput, FlowContentSummary, FlowUpdate},
    },
    message::{Message, MessageOrigin, MessagePart},
};

/// Stable position within one flow journal. Viewports and interaction revisions do not affect it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
pub struct FlowRecordId {
    pub commit: u64,
    pub fact: usize,
    pub item: usize,
}

/// Original conversation, execution, and checkpoint facts remain distinct.
#[derive(Clone, Debug)]
pub enum FlowRecord {
    Input(Box<CommittedInput>),
    Publication(Box<MessagePublication>),
    Execution(Box<ExecutionEvent>),
    Checkpoint(Box<ContextCheckpoint>),
    Completion(Box<Completion>),
    Legacy(Box<LegacyHistoryRecord>),
    Continuation {
        source: Box<crate::flow_store::history::LegacyArchive>,
        messages: Vec<Message>,
    },
}

impl FlowRecord {
    /// Messages belonging to this instance's shared conversation. An input commit does not imply that a model has read it.
    pub fn shared_messages(&self) -> &[Message] {
        match self {
            Self::Continuation { messages, .. } => messages,
            Self::Input(input) => shared_input_messages(input),
            Self::Publication(publication) => shared_publication_messages(publication),
            _ => &[],
        }
    }
}

impl FlowUpdate {
    /// Borrow committed conversation messages without retaining a document or execution trace.
    pub fn shared_messages(&self) -> &[Message] {
        match self {
            Self::Activated {
                continued_from: Some(_),
                inherited_context,
                ..
            } => inherited_context,
            Self::InputCommitted(input) => shared_input_messages(input),
            Self::MessagePublished(publication) => shared_publication_messages(publication),
            _ => &[],
        }
    }
}

fn shared_input_messages(input: &CommittedInput) -> &[Message] {
    match &input.content {
        InputContent::Context { messages } | InputContent::InvocationInput { messages, .. } => {
            messages
        }
        InputContent::WaitingContinuation { .. } => &[],
    }
}

fn shared_publication_messages(publication: &MessagePublication) -> &[Message] {
    if publication.context_run.is_none() {
        std::slice::from_ref(&publication.message)
    } else {
        &[]
    }
}

#[derive(Clone, Debug)]
pub struct FlowTranscriptEntry {
    pub id: FlowRecordId,
    pub record: FlowRecord,
}

/// One append-only document source shared by views of a persistent instance.
///
/// Apply complete batches in order. Repeated receipts are idempotent; gaps, foreign journals, and conflicting receipts fail before changing the projection. This reader neither consumes inbox entries nor changes execution state. Checkpoints retain the original conversation. Ordinary inherited model context and accepted-but-uncommitted input do not become transcript messages; explicit historical continuations expose their inherited context with source provenance.
pub struct FlowTranscript {
    flow: FlowRef,
    selection: Option<std::sync::Arc<crate::flow_store::branches::ConversationView>>,
    initial_head: Option<crate::flow_store::branches::HistoryHead>,
    content: Option<FlowContentSummary>,
    receipts: Vec<FlowPosition>,
    entries: Vec<FlowTranscriptEntry>,
    workflows: FlowWorkflows,
    startup_input: Option<uuid::Uuid>,
    work_input: bool,
    peer_inputs: BTreeMap<uuid::Uuid, Box<AcceptedEnvelope>>,
}

impl FlowTranscript {
    pub fn new(flow: FlowRef) -> Self {
        Self {
            flow,
            selection: None,
            initial_head: None,
            content: None,
            receipts: Vec::new(),
            entries: Vec::new(),
            workflows: FlowWorkflows::default(),
            startup_input: None,
            work_input: false,
            peer_inputs: BTreeMap::new(),
        }
    }

    pub fn flow(&self) -> &FlowRef {
        &self.flow
    }

    pub fn history_head(&self) -> Option<&crate::flow_store::branches::HistoryHead> {
        self.selection
            .as_ref()
            .map(|selection| selection.head())
            .or(self.initial_head.as_ref())
    }

    pub(crate) fn bind_selection(
        &mut self,
        selection: Option<std::sync::Arc<crate::flow_store::branches::ConversationView>>,
    ) -> io::Result<()> {
        if let Some(selection) = &selection {
            if selection.flow() != &self.flow
                || self.cursor().is_some_and(|cursor| {
                    self.history_head() != Some(selection.head())
                        || !cursor.same_history(selection.through())
                        || cursor.sequence() > selection.through().sequence()
                })
            {
                return Err(invalid(
                    "conversation selection does not extend this transcript",
                ));
            }
        } else if self.selection.is_some() {
            return Err(invalid("conversation selection cannot be removed"));
        }
        self.selection = selection;
        Ok(())
    }

    pub fn content(&self) -> Option<&FlowContentSummary> {
        self.content.as_ref()
    }

    pub fn cursor(&self) -> Option<&FlowPosition> {
        self.receipts.last()
    }

    /// Check that an earlier view cursor belongs to this exact committed history.
    pub fn contains_cursor(&self, cursor: &FlowPosition) -> bool {
        cursor
            .sequence()
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| self.receipts.get(index))
            == Some(cursor)
    }

    pub fn entries(&self) -> &[FlowTranscriptEntry] {
        &self.entries
    }

    /// Whether input other than the captured initialization command has entered execution. Sender identity and rendered message roles do not determine whether work began.
    pub fn has_work_input(&self) -> bool {
        self.work_input
    }

    pub fn turns(&self) -> impl DoubleEndedIterator<Item = &FlowTurnProjection> {
        self.workflows.turns()
    }

    /// Append one validated batch and return its new entry range. Cursor advancement and all entries become visible together.
    pub fn apply(&mut self, batch: FlowBatch<FlowUpdate>) -> io::Result<Range<usize>> {
        if self.selection.is_none()
            && batch
                .facts
                .iter()
                .any(|fact| matches!(fact, FlowUpdate::HistoryHeadChanged(_)))
        {
            return Err(crate::flow_store::branches::unsupported());
        }
        let start = self.entries.len();
        let receipt = &batch.receipt;
        if self.selection.as_ref().is_some_and(|selection| {
            !receipt.same_history(selection.through())
                || receipt.sequence() > selection.through().sequence()
                || (receipt.sequence() == selection.through().sequence()
                    && receipt != selection.through())
        }) {
            return Err(invalid(
                "transcript batch exceeds its captured conversation selection",
            ));
        }
        let visible = self
            .selection
            .as_ref()
            .is_none_or(|selection| selection.contains(receipt.sequence()));
        if receipt.journal_id() != self.flow.instance_id.0.0
            || receipt.sequence() == 0
            || self
                .receipts
                .first()
                .is_some_and(|first| !first.same_history(receipt))
        {
            return Err(invalid(
                "transcript receipt belongs to another journal or has no position",
            ));
        }
        let position = usize::try_from(receipt.sequence() - 1)
            .map_err(|_| invalid("transcript receipt position is too large"))?;
        if let Some(saved) = self.receipts.get(position) {
            return if saved == receipt {
                Ok(start..start)
            } else {
                Err(invalid(
                    "transcript receipt conflicts with an applied batch",
                ))
            };
        }
        if position != self.receipts.len() || batch.facts.is_empty() {
            return Err(invalid(
                "transcript requires the next complete committed batch",
            ));
        }
        if self.content.is_none()
            && !matches!(batch.facts.as_slice(), [FlowUpdate::Created { flow, .. }] if flow == &self.flow)
        {
            return Err(invalid("transcript must start with its instance creation"));
        }
        // Check transcript-bearing owners before exposing new entries. The journal remains the authority for fact validation.
        for fact in &batch.facts {
            let owner = match fact {
                FlowUpdate::Created { flow, .. } => {
                    if self.content.is_some() {
                        return Err(invalid("transcript instance was already created"));
                    }
                    Some(flow)
                }
                FlowUpdate::Activated {
                    flow,
                    input,
                    startup_input,
                    ..
                } => {
                    if input
                        .as_ref()
                        .is_some_and(|input| input.recipient() != flow)
                    {
                        return Err(invalid("activation input belongs to another flow"));
                    }
                    if startup_input.is_some_and(|startup| !matches!(input.as_deref(), Some(crate::flow_store::view::AcceptedInput::Command { id, .. }) if *id == startup)) {
                        return Err(invalid("startup input does not identify the activation command"));
                    }
                    Some(flow)
                }
                FlowUpdate::InputAccepted(input) => Some(input.recipient()),
                FlowUpdate::InputCommitted(input) => Some(&input.lease.flow),
                FlowUpdate::MessagePublished(publication) => Some(&publication.lease.flow),
                FlowUpdate::TurnCompleted(completion) => Some(&completion.lease.flow),
                FlowUpdate::ExecutionEvents(events) => {
                    if events.iter().any(|event| event.scope.flow() != &self.flow) {
                        return Err(invalid("execution record belongs to another flow"));
                    }
                    None
                }
                _ => None,
            };
            if owner.is_some_and(|owner| owner != &self.flow) {
                return Err(invalid("transcript fact belongs to another flow"));
            }
        }
        for (fact_index, fact) in batch.facts.into_iter().enumerate() {
            let mut append = |item, record| {
                if !visible {
                    return;
                }
                self.entries.push(FlowTranscriptEntry {
                    id: FlowRecordId {
                        commit: receipt.sequence(),
                        fact: fact_index,
                        item,
                    },
                    record,
                })
            };
            match fact {
                FlowUpdate::Created { content, .. } => {
                    if !matches!(content, FlowContentSummary::History(_))
                        && let FlowPosition::Durable(receipt) = receipt
                    {
                        self.initial_head =
                            Some(crate::flow_store::branches::HistoryHead::initial(
                                &self.flow,
                                receipt.journal_id,
                            ));
                    }
                    self.content = Some(content);
                }
                FlowUpdate::Activated {
                    startup_input,
                    input,
                    continued_from,
                    inherited_context,
                    ..
                } => {
                    self.startup_input = startup_input;
                    if let Some(input) = input {
                        retain_peer_input(&mut self.peer_inputs, *input);
                    }
                    if let Some(source) = continued_from {
                        append(
                            0,
                            FlowRecord::Continuation {
                                source: Box::new(source),
                                messages: inherited_context,
                            },
                        );
                    }
                }
                FlowUpdate::InputAccepted(input) => {
                    retain_peer_input(&mut self.peer_inputs, *input)
                }
                FlowUpdate::InputScopeInvalidated(value) => {
                    self.peer_inputs.retain(|_, envelope| {
                        envelope.request.scope.as_ref() != Some(&value.scope)
                    });
                }
                FlowUpdate::InputCommitted(mut value) => {
                    present_peer_inputs(&mut self.peer_inputs, &mut value);
                    self.work_input |= visible
                        && value
                            .message_ids
                            .iter()
                            .any(|id| Some(*id) != self.startup_input);
                    append(0, FlowRecord::Input(value));
                }
                FlowUpdate::MessagePublished(value) => append(0, FlowRecord::Publication(value)),
                FlowUpdate::ContextCheckpoint(value) => append(0, FlowRecord::Checkpoint(value)),
                FlowUpdate::TurnCompleted(value) => append(0, FlowRecord::Completion(value)),
                FlowUpdate::ExecutionEvents(events) => {
                    for (index, event) in events.into_iter().enumerate() {
                        append(index, FlowRecord::Execution(Box::new(event)));
                    }
                }
                FlowUpdate::LegacyRecords(records) => {
                    for (index, record) in records.into_iter().enumerate() {
                        append(index, FlowRecord::Legacy(Box::new(record)));
                    }
                }
                _ => {}
            }
        }
        for entry in &self.entries[start..] {
            self.workflows.observe(entry);
        }
        self.workflows.finish_batch();
        self.receipts.push(batch.receipt);
        Ok(start..self.entries.len())
    }
}

fn retain_peer_input(
    inputs: &mut BTreeMap<uuid::Uuid, Box<AcceptedEnvelope>>,
    input: AcceptedInput,
) {
    if let AcceptedInput::Envelope(envelope) = input
        && matches!(envelope.request.sender, SenderIdentity::Flow(_))
        && matches!(envelope.request.payload, InboxPayload::Interjection { .. })
    {
        inputs.insert(envelope.id, envelope);
    }
}

fn present_peer_inputs(
    inputs: &mut BTreeMap<uuid::Uuid, Box<AcceptedEnvelope>>,
    input: &mut CommittedInput,
) {
    for id in &input.message_ids {
        let Some(envelope) = inputs.remove(id) else {
            continue;
        };
        let Ok(projected) = crate::flow_store::inbox::context_message(&envelope, &input.lease.turn)
        else {
            continue;
        };
        let messages = match &mut input.content {
            InputContent::Context { messages } | InputContent::InvocationInput { messages, .. } => {
                messages
            }
            InputContent::WaitingContinuation { .. } => continue,
        };
        // Accepted identities recover display provenance for older messages without parsing prompt text or changing stored/model context.
        if let Some(message) = messages.iter_mut().find(|message| {
            message.role == projected.role
                && matches!(
                    message.origin,
                    MessageOrigin::Interjection | MessageOrigin::Peer
                )
                && message.text_concat() == projected.text_concat()
        }) {
            message.origin = MessageOrigin::Peer;
            message
                .parts
                .retain(|part| !matches!(part, MessagePart::PeerInput { .. }));
            message.parts.extend(
                projected
                    .parts
                    .into_iter()
                    .filter(|part| matches!(part, MessagePart::PeerInput { .. })),
            );
        }
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
