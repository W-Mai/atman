//! Execution diagnostics in the instance journal, separate from conversation and control facts.

use std::io;

use atman_rt::{FlowInstance, InstanceActivity, InstanceState, TurnLease};
use serde::{Deserialize, Serialize};

use super::{FlowFact, FlowStore, completion::Completions, invalid};
use crate::event::{AtmanUuid, Event};
use crate::event_log::journal::CommitReceipt;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecutionEvent {
    pub lease: TurnLease<AtmanUuid>,
    pub sequence: u64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub event: Event,
}

#[derive(Clone, Debug, Serialize)]
pub struct ExecutionEventBatch {
    pub events: Vec<ExecutionEvent>,
    pub receipt: CommitReceipt,
}

#[derive(Default)]
pub(super) struct ExecutionTrace {
    batches: Vec<ExecutionEventBatch>,
    sequence: u64,
}

impl ExecutionTrace {
    pub fn validate(
        &self,
        events: &[ExecutionEvent],
        state: &FlowInstance<AtmanUuid>,
        completions: &Completions,
    ) -> io::Result<()> {
        if events.is_empty() {
            return Err(invalid("execution event batch is empty"));
        }
        let mut sequence = self.sequence;
        for event in events {
            let active = match state.state() {
                InstanceState::Open(InstanceActivity::Active(active))
                | InstanceState::Closing {
                    active: Some(active),
                } => Some(&active.lease),
                _ => None,
            };
            let known = active == Some(&event.lease)
                || completions
                    .get(&event.lease)
                    .is_some_and(|record| record.completion.lease == event.lease);
            if &event.lease.flow != state.address()
                || !known
                || event.sequence <= sequence
                || !is_execution_event(&event.event)
            {
                return Err(invalid(
                    "execution event has an invalid owner, sequence, or kind",
                ));
            }
            sequence = event.sequence;
        }
        Ok(())
    }

    pub fn apply(&mut self, events: Vec<ExecutionEvent>, receipt: CommitReceipt) {
        self.sequence = events
            .last()
            .expect("validated nonempty execution batch")
            .sequence;
        self.batches.push(ExecutionEventBatch { events, receipt });
    }
}

impl FlowStore {
    pub fn execution_events(&self) -> &[ExecutionEventBatch] {
        &self.trace.batches
    }

    /// Append trusted execution diagnostics; message bodies and lifecycle changes use their own facts. Sequence positions belong to this instance's event sink, while the returned receipt identifies the durable batch.
    pub fn append_execution_events(
        &mut self,
        events: Vec<ExecutionEvent>,
    ) -> io::Result<CommitReceipt> {
        self.trace
            .validate(&events, &self.state, &self.completions)?;
        let receipt = self
            .journal
            .commit(&[FlowFact::ExecutionEvents(events.clone())])?;
        self.trace.apply(events, receipt.clone());
        self.cursor = receipt.clone();
        // Execution diagnostics do not change the descriptor/state projection.
        Ok(receipt)
    }
}

pub(crate) fn is_execution_event(event: &Event) -> bool {
    match event {
        Event::FlowStart { .. }
        | Event::FlowEnd { .. }
        | Event::WorkspaceLifecycle { .. }
        | Event::LlmCall { .. }
        | Event::ToolResultMetrics { .. }
        | Event::DiffPreview { .. }
        | Event::FileEditApplied { .. }
        | Event::ContentFilterHit { .. }
        | Event::ContextTruncated { .. }
        | Event::WatchWarn { .. }
        | Event::LlmPartialCall { .. }
        | Event::PendingPrompt { .. }
        | Event::PromptExpired { .. }
        | Event::PromptResolved { .. }
        | Event::FlowGraph { .. }
        | Event::FlowNodeStart { .. }
        | Event::FlowNodeEnd { .. }
        | Event::ToolNode { .. }
        | Event::AttachmentDegraded { .. }
        | Event::ToolPendingApproval { .. }
        | Event::ToolApproved { .. }
        | Event::ToolDenied { .. }
        | Event::PermissionRequestCreated { .. }
        | Event::PermissionRequestTargeted { .. }
        | Event::PermissionRequestDeferred { .. }
        | Event::PermissionRequestApproved { .. }
        | Event::PermissionRequestDenied { .. }
        | Event::PermissionRequestCancelled { .. }
        | Event::PermissionGroupCreated { .. }
        | Event::PermissionGroupUpdated { .. }
        | Event::PermissionGroupResolved { .. }
        | Event::PermissionGrantCreated { .. }
        | Event::PermissionGrantExpired { .. }
        | Event::UnrestrictedExecution { .. }
        | Event::TerminalFinalState { .. }
        | Event::MermaidDiagram { .. } => true,
        Event::GoalTurnStarted { .. }
        | Event::GoalChanged { .. }
        | Event::TurnStart { .. }
        | Event::TurnEnd { .. }
        | Event::UserMsg { .. }
        | Event::AssistantMsg { .. }
        | Event::ToolResultMsg { .. }
        | Event::SystemMsg { .. }
        | Event::UserInject { .. }
        | Event::CompactionSummary { .. }
        | Event::ContextCompact { .. }
        | Event::Checkpoint { .. }
        | Event::DeferredFormRecorded { .. }
        | Event::DeferredFormApplied { .. } => false,
    }
}
