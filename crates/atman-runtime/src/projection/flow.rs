//! Instance-scoped transcript projection from committed journal batches.

use std::{io, ops::Range};

use serde::{Deserialize, Serialize};

use super::flow_workflow::FlowWorkflows;
pub use super::flow_workflow::{FlowTurnEnd, FlowTurnProjection};

use crate::{
    event_log::journal::{CommitReceipt, CommittedBatch},
    flow_store::{
        FlowRef,
        completion::Completion,
        context::ContextCheckpoint,
        history::LegacyHistoryRecord,
        inbox::{CommittedInput, InputContent},
        publication::MessagePublication,
        trace::ExecutionEvent,
        view::{FlowContentSummary, FlowUpdate},
    },
    message::Message,
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
}

impl FlowRecord {
    /// Messages belonging to this instance's shared conversation. An input commit does not imply that a model has read it.
    pub fn shared_messages(&self) -> &[Message] {
        match self {
            Self::Input(input) => match &input.content {
                InputContent::Context { messages }
                | InputContent::InvocationInput { messages, .. } => messages,
                InputContent::WaitingContinuation { .. } => &[],
            },
            Self::Publication(publication) if publication.context_run.is_none() => {
                std::slice::from_ref(&publication.message)
            }
            _ => &[],
        }
    }
}

#[derive(Clone, Debug)]
pub struct FlowTranscriptEntry {
    pub id: FlowRecordId,
    pub record: FlowRecord,
}

/// One append-only document source shared by views of a persistent instance.
///
/// Apply complete batches in order. Repeated receipts are idempotent; gaps, foreign journals, and conflicting receipts fail before changing the projection. This reader neither consumes inbox entries nor changes execution state. Checkpoints retain the original conversation; inherited model context and accepted-but-uncommitted input do not become transcript messages.
pub struct FlowTranscript {
    flow: FlowRef,
    content: Option<FlowContentSummary>,
    receipts: Vec<CommitReceipt>,
    entries: Vec<FlowTranscriptEntry>,
    workflows: FlowWorkflows,
}

impl FlowTranscript {
    pub fn new(flow: FlowRef) -> Self {
        Self {
            flow,
            content: None,
            receipts: Vec::new(),
            entries: Vec::new(),
            workflows: FlowWorkflows::default(),
        }
    }

    pub fn flow(&self) -> &FlowRef {
        &self.flow
    }

    pub fn content(&self) -> Option<&FlowContentSummary> {
        self.content.as_ref()
    }

    pub fn cursor(&self) -> Option<&CommitReceipt> {
        self.receipts.last()
    }

    /// Check that an earlier view cursor belongs to this exact committed history.
    pub fn contains_cursor(&self, cursor: &CommitReceipt) -> bool {
        cursor
            .sequence
            .checked_sub(1)
            .and_then(|index| usize::try_from(index).ok())
            .and_then(|index| self.receipts.get(index))
            == Some(cursor)
    }

    pub fn entries(&self) -> &[FlowTranscriptEntry] {
        &self.entries
    }

    pub fn turns(&self) -> impl DoubleEndedIterator<Item = &FlowTurnProjection> {
        self.workflows.turns()
    }

    /// Append one validated batch and return its new entry range. Cursor advancement and all entries become visible together.
    pub fn apply(&mut self, batch: CommittedBatch<FlowUpdate>) -> io::Result<Range<usize>> {
        let start = self.entries.len();
        let receipt = &batch.receipt;
        if receipt.journal_id != self.flow.instance_id.0.0 || receipt.sequence == 0 {
            return Err(invalid(
                "transcript receipt belongs to another journal or has no position",
            ));
        }
        let position = usize::try_from(receipt.sequence - 1)
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
                FlowUpdate::CommandAccepted { recipient, .. } => Some(recipient),
                FlowUpdate::Activated { flow, envelope, .. } => {
                    if envelope
                        .as_ref()
                        .is_some_and(|envelope| &envelope.request.recipient != flow)
                    {
                        return Err(invalid("activation input belongs to another flow"));
                    }
                    Some(flow)
                }
                FlowUpdate::EnvelopeAccepted(envelope) => Some(&envelope.request.recipient),
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
                self.entries.push(FlowTranscriptEntry {
                    id: FlowRecordId {
                        commit: receipt.sequence,
                        fact: fact_index,
                        item,
                    },
                    record,
                })
            };
            match fact {
                FlowUpdate::Created { content, .. } => self.content = Some(content),
                FlowUpdate::InputCommitted(value) => append(0, FlowRecord::Input(value)),
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

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}
