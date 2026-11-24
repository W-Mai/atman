//! Execution diagnostics in the instance journal, separate from conversation and control facts.

use std::io;

use atman_rt::{FlowInstance, InstanceActivity, InstanceState, TurnLease};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{FlowFact, FlowRef, FlowStore, completion::Completions, invalid};
use crate::event::{AtmanUuid, Event};
use crate::flow_store::position::FlowPosition;
use crate::permission::{MAX_USER_APPROVAL_BATCH, PermissionAction};
use crate::permission_audit::{
    PermissionAuditScope, PermissionAuditTarget, PermissionProjectionActor,
};

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
    pub receipt: FlowPosition,
}

/// An original scoped User decision whose complete operation is present in the instance journal.
#[derive(Debug, PartialEq, Eq)]
pub struct RecordedApprovalDecision {
    pub request_id: Uuid,
    pub decision_id: Uuid,
    pub action: PermissionAction,
}

pub(super) struct ApprovalReceiptQuery<'a> {
    flow: &'a FlowRef,
    session: String,
    principal: String,
    operation_id: Uuid,
    observed: bool,
    pub decisions: Option<Vec<RecordedApprovalDecision>>,
}

impl<'a> ApprovalReceiptQuery<'a> {
    pub fn new(flow: &'a FlowRef, principal: Uuid, operation_id: Uuid) -> Self {
        Self {
            flow,
            session: flow.session_id.to_string(),
            principal: principal.to_string(),
            operation_id,
            observed: false,
            decisions: None,
        }
    }

    pub fn observe(&mut self, events: &[ExecutionEvent]) {
        let mut members = events
            .iter()
            .filter_map(|event| {
                let (audit, action) = match &event.event {
                    Event::PermissionRequestApproved { payload } => {
                        (payload, Some(PermissionAction::Approve))
                    }
                    Event::PermissionRequestDenied { payload } => {
                        (payload, Some(PermissionAction::Deny))
                    }
                    Event::PermissionRequestCancelled { payload } => {
                        (payload, Some(PermissionAction::Defer))
                    }
                    Event::PermissionRequestDeferred { payload } => (payload, None),
                    _ => return None,
                };
                let member = audit.user_approval.as_ref()?;
                let Some(PermissionProjectionActor::User {
                    session_id,
                    principal_id: Some(principal),
                }) = &audit.actor
                else {
                    return None;
                };
                (member.operation_id == self.operation_id && principal == &self.principal)
                    .then_some((event, audit, member, session_id, action))
            })
            .peekable();
        let Some((_, _, first, _, _)) = members.peek() else {
            return;
        };
        let count = first.count;
        self.decisions = None;
        if std::mem::replace(&mut self.observed, true)
            || !(1..=MAX_USER_APPROVAL_BATCH).contains(&count)
        {
            return;
        }
        let mut decisions: Vec<RecordedApprovalDecision> = Vec::with_capacity(count);
        for (index, (event, audit, member, session, action)) in members.enumerate() {
            let (Some(request), Some(decision), Some(action)) =
                (&audit.request_id, &audit.decision_id, action)
            else {
                return;
            };
            let Ok(decision_id) = Uuid::parse_str(decision) else {
                return;
            };
            let request_id = request.0;
            if !matches!(&event.scope, ExecutionScope::Instance(flow) if flow == self.flow)
                || session != &self.session
                || audit.session_id != self.session
                || audit.target != PermissionAuditTarget::User
                || member.count != count
                || member.index != index
                || index >= count
                || request_id.is_nil()
                || decision_id.is_nil()
                || match action {
                    PermissionAction::Approve => {
                        audit.scope != Some(PermissionAuditScope::CurrentCall)
                    }
                    PermissionAction::Deny | PermissionAction::Defer => audit.scope.is_some(),
                }
                || decisions.iter().any(|other| {
                    other.request_id == request_id
                        || other.decision_id == decision_id
                        || other.action != action
                })
            {
                return;
            }
            decisions.push(RecordedApprovalDecision {
                request_id,
                decision_id,
                action,
            });
        }
        if decisions.len() == count {
            self.decisions = Some(decisions);
        }
    }
}

#[derive(Default)]
pub(super) struct ExecutionTrace {
    batches: Vec<ExecutionEventBatch>,
    sequence: u64,
    pub(super) latest: Option<super::execution::ExecutionEventRef>,
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

    pub fn apply(&mut self, events: Vec<ExecutionEvent>, receipt: FlowPosition) {
        for event in &events {
            if let Some(event) = super::execution::ExecutionEventRef::capture(event, &receipt)
                && self
                    .latest
                    .as_ref()
                    .is_none_or(|latest| event.lease.epoch >= latest.lease.epoch)
            {
                self.latest = Some(event);
            }
        }
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
    ) -> io::Result<FlowPosition> {
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
        | Event::BashLogCheckpoint { .. }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::FlowRunId;
    use crate::permission::PermissionRequestId;
    use crate::permission_audit::{
        PermissionPolicyReference, PermissionRequestAudit, UserApprovalAuditMember,
    };
    use crate::tool::Tier;

    #[test]
    fn approval_receipt_requires_one_complete_ordered_batch_from_the_original_device_and_flow() {
        let flow = FlowRef {
            session_id: Uuid::new_v4().into(),
            instance_id: super::super::FlowInstanceId::now(),
        };
        let principal = Uuid::new_v4();
        let operation = Uuid::new_v4();
        let session = flow.session_id.to_string();
        let make = |index: usize| {
            let timestamp = chrono::Utc::now();
            let run = FlowRunId::now();
            ExecutionEvent {
                scope: ExecutionScope::Instance(flow.clone()),
                sequence: (index + 1) as u64,
                timestamp,
                event: Event::PermissionRequestApproved {
                    payload: PermissionRequestAudit {
                        request_id: Some(PermissionRequestId::now()),
                        revision: 2,
                        session_id: session.clone(),
                        requesting_run_id: run.clone(),
                        parent_run_id: None,
                        root_run_id: run,
                        tool_use_id: format!("tool-{index}"),
                        tool: "fs.write".into(),
                        call_intent: None,
                        tier: Tier::Two,
                        execution_boundary: None,
                        provenance: Default::default(),
                        target: PermissionAuditTarget::User,
                        group_ids: Vec::new(),
                        policy: PermissionPolicyReference {
                            snapshot_id: "snapshot".into(),
                            rule_id: "rule".into(),
                        },
                        escalation_path: Vec::new(),
                        decision_id: Some(Uuid::new_v4().to_string()),
                        user_approval: Some(UserApprovalAuditMember {
                            operation_id: operation,
                            index,
                            count: 2,
                        }),
                        actor: Some(PermissionProjectionActor::User {
                            session_id: session.clone(),
                            principal_id: Some(principal.to_string()),
                        }),
                        scope: Some(PermissionAuditScope::CurrentCall),
                        reason: None,
                        at: timestamp,
                    },
                },
            }
        };
        let originals = [make(0), make(1)];
        let mut query = ApprovalReceiptQuery::new(&flow, principal, operation);
        query.observe(&originals);
        let exact = query.decisions.as_ref().unwrap();
        assert_eq!(exact.len(), 2);
        for (decision, original) in exact.iter().zip(&originals) {
            let Event::PermissionRequestApproved { payload } = &original.event else {
                unreachable!()
            };
            assert_eq!(decision.request_id, payload.request_id.as_ref().unwrap().0);
            assert_eq!(
                decision.decision_id.to_string(),
                *payload.decision_id.as_ref().unwrap()
            );
        }
        query.observe(&originals);
        assert!(
            query.decisions.is_none(),
            "a repeated operation is ambiguous"
        );

        let mut split = ApprovalReceiptQuery::new(&flow, principal, operation);
        split.observe(&originals[..1]);
        split.observe(&originals[1..]);
        assert!(split.decisions.is_none());
        let mut reversed = ApprovalReceiptQuery::new(&flow, principal, operation);
        reversed.observe(&[originals[1].clone(), originals[0].clone()]);
        assert!(reversed.decisions.is_none());
        let mut other_device = ApprovalReceiptQuery::new(&flow, Uuid::new_v4(), operation);
        other_device.observe(&originals);
        assert!(other_device.decisions.is_none());
        let other_flow = FlowRef {
            instance_id: super::super::FlowInstanceId::now(),
            ..flow.clone()
        };
        let mut other = ApprovalReceiptQuery::new(&other_flow, principal, operation);
        other.observe(&originals);
        assert!(other.decisions.is_none());
    }
}
