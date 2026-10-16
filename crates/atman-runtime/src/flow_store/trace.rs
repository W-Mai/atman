//! Execution diagnostics in the instance journal, separate from conversation and control facts.

use std::io;

use atman_rt::{FlowInstance, InstanceActivity, InstanceState, TurnLease};
use serde::{Deserialize, Serialize};

use super::{FlowFact, FlowRef, FlowStore, completion::Completions, invalid};
use crate::event::{AtmanUuid, Event};
use crate::event_log::journal::CommitReceipt;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ExecutionEvent {
    pub scope: ExecutionScope,
    pub sequence: u64,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub event: Event,
}

/// Service diagnostics belong to the instance, even when no turn is active. Invocation diagnostics retain their original lease after completion.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "owner", rename_all = "snake_case")]
pub enum ExecutionScope {
    Invocation(TurnLease<AtmanUuid>),
    Instance(FlowRef),
}

impl ExecutionScope {
    pub fn flow(&self) -> &FlowRef {
        match self {
            Self::Invocation(lease) => &lease.flow,
            Self::Instance(address) => address,
        }
    }

    pub fn lease(&self) -> Option<&TurnLease<AtmanUuid>> {
        match self {
            Self::Invocation(lease) => Some(lease),
            Self::Instance(_) => None,
        }
    }

    pub(crate) fn accepts(&self, event: &Event) -> bool {
        match self {
            Self::Invocation(_) => is_execution_event(event),
            Self::Instance(_) => is_service_event(event),
        }
    }
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
            let known = event.scope.lease().is_none_or(|lease| {
                active == Some(lease)
                    || completions
                        .get(lease)
                        .is_some_and(|record| &record.completion.lease == lease)
            });
            if event.scope.flow() != state.address()
                || !known
                || event.sequence <= sequence
                || !event.scope.accepts(&event.event)
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

fn is_service_event(event: &Event) -> bool {
    matches!(
        event,
        Event::PermissionRequestCreated { .. }
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
    )
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
