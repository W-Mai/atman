//! Durable execution controls with actor-scoped retry receipts.

use std::{collections::HashMap, io};

use atman_rt::{FlowInstance, InstanceActivity, InstanceState, TurnPhase};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{FlowFact, FlowRef, FlowStore, FlowTransition, inbox::SenderIdentity, invalid};
use crate::{event::AtmanUuid, flow_store::position::FlowPosition};

/// Controls retain invocation identity and independent pause ownership. Only explicit operator release can remove another controller's key.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAction {
    Pause,
    Resume,
    /// A trusted user or host releases one explicitly selected, currently recorded pause.
    ReleasePause,
    CancelTurn,
    Recover,
    Close,
}

/// The host authenticates the sender and binds its controller key or explicit operator selection before submission.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlRequest {
    pub id: Uuid,
    pub sender: SenderIdentity,
    pub target: FlowRef,
    pub expected_revision: u64,
    pub pause_key: AtmanUuid,
    pub action: ControlAction,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ControlUpdate {
    pub request: ControlRequest,
    pub at: chrono::DateTime<chrono::Utc>,
}

/// Execution is the state at acceptance, not a claim that asynchronous cleanup has finished.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CommittedControl {
    pub update: ControlUpdate,
    #[serde(with = "super::execution::instance_wire")]
    pub execution: FlowInstance<AtmanUuid>,
    pub receipt: FlowPosition,
}

#[derive(Default)]
pub(super) struct Controls(HashMap<(SenderIdentity, Uuid), CommittedControl>);

impl Controls {
    pub(super) fn previous(
        &self,
        request: &ControlRequest,
    ) -> io::Result<Option<&CommittedControl>> {
        match self.0.get(&(request.sender.clone(), request.id)) {
            Some(record) if &record.update.request != request => Err(invalid(
                "control request identity conflicts with recorded data",
            )),
            record => Ok(record),
        }
    }

    pub(super) fn prepare(
        &self,
        state: &FlowInstance<AtmanUuid>,
        request: &ControlRequest,
    ) -> io::Result<FlowInstance<AtmanUuid>> {
        if request.id.is_nil()
            || request.pause_key.0.is_nil()
            || &request.target != state.address()
            || request.expected_revision != state.revision()
            || self.previous(request)?.is_some()
        {
            return Err(invalid("stale or invalid execution control"));
        }
        let mut candidate = state.clone();
        let mut transition = match request.action {
            ControlAction::Pause => FlowTransition::ExecutionPaused {
                key: request.pause_key,
            },
            ControlAction::Resume | ControlAction::ReleasePause => {
                if request.action == ControlAction::ReleasePause {
                    if matches!(request.sender, SenderIdentity::Flow(_)) {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "only an operator may release another controller's pause",
                        ));
                    }
                    if !state.pauses().contains(&request.pause_key) {
                        return Err(invalid("selected pause is no longer present"));
                    }
                }
                FlowTransition::ExecutionResumed {
                    key: request.pause_key,
                }
            }
            ControlAction::Recover => FlowTransition::InterruptionRecovered,
            ControlAction::Close => FlowTransition::CloseRequested,
            ControlAction::CancelTurn => match state.state() {
                InstanceState::Open(InstanceActivity::Active(active)) => {
                    FlowTransition::PhaseChanged {
                        lease: active.lease.clone(),
                        phase: TurnPhase::Cancelling,
                    }
                }
                _ => return Ok(candidate),
            },
        };
        transition.apply(&mut candidate)?;
        Ok(candidate)
    }

    pub(super) fn apply(
        &mut self,
        update: ControlUpdate,
        execution: FlowInstance<AtmanUuid>,
        receipt: FlowPosition,
    ) -> CommittedControl {
        let record = CommittedControl {
            update,
            execution,
            receipt,
        };
        self.0.insert(
            (
                record.update.request.sender.clone(),
                record.update.request.id,
            ),
            record.clone(),
        );
        record
    }
}

/// A body-free read of the committed execution state and one retained operator control.
#[derive(Debug)]
pub struct ExecutionControlObservation {
    pub execution: FlowInstance<AtmanUuid>,
    pub cursor: FlowPosition,
    pub loaded: bool,
    pub control: Option<ExecutionControlReceipt>,
}

#[derive(Debug)]
pub struct ExecutionControlReceipt {
    pub action: ControlAction,
    pub accepted: FlowPosition,
    pub applied: Option<FlowPosition>,
}

impl Controls {
    fn execution_receipt(
        &self,
        sender: &SenderIdentity,
        request_id: Option<Uuid>,
        completions: &super::completion::Completions,
    ) -> Option<ExecutionControlReceipt> {
        let record = self.0.get(&(sender.clone(), request_id?))?;
        let action = record.update.request.action;
        let applied = match action {
            ControlAction::CancelTurn => cancel_application_receipt(record, completions),
            ControlAction::Recover => Some(record.receipt.clone()),
            _ => return None,
        };
        Some(ExecutionControlReceipt {
            action,
            accepted: record.receipt.clone(),
            applied,
        })
    }
}

fn cancel_application_receipt(
    accepted: &CommittedControl,
    completions: &super::completion::Completions,
) -> Option<FlowPosition> {
    match accepted.execution.state() {
        InstanceState::Open(InstanceActivity::Active(active)) => completions
            .get(&active.lease)
            .filter(|done| done.completion.lease == active.lease)
            .map(|done| done.receipt.clone()),
        // A no-op cancellation has no invocation cleanup to wait for.
        _ => Some(accepted.receipt.clone()),
    }
}

impl FlowStore {
    pub(crate) fn cancel_application_receipt(
        &self,
        accepted: &CommittedControl,
    ) -> Option<FlowPosition> {
        cancel_application_receipt(accepted, &self.completions)
    }

    pub(crate) fn execution_controls(
        &self,
        sender: &SenderIdentity,
        request_id: Option<Uuid>,
    ) -> ExecutionControlObservation {
        ExecutionControlObservation {
            execution: self.state().clone(),
            cursor: self.cursor().clone(),
            loaded: true,
            control: self
                .controls
                .execution_receipt(sender, request_id, &self.completions),
        }
    }
}

impl super::SessionInspection {
    pub(crate) fn execution_controls(
        &self,
        id: &super::FlowInstanceId,
        sender: &SenderIdentity,
        request_id: Option<Uuid>,
    ) -> io::Result<ExecutionControlObservation> {
        let read = super::read_registered_flow(&self.dir, &self.projection, id, |_, _| Ok(()))?;
        let replay = read.replay;
        let (_, execution, _) = replay
            .projection
            .ok_or_else(|| invalid("missing execution projection"))?;
        Ok(ExecutionControlObservation {
            execution,
            cursor: replay
                .cursor
                .ok_or_else(|| invalid("missing execution receipt"))?,
            loaded: false,
            control: replay
                .controls
                .execution_receipt(sender, request_id, &replay.completions),
        })
    }
}

impl FlowStore {
    /// Trusted host reconciliation of an already admitted command; this never mutates execution.
    pub(crate) fn recorded_control(
        &self,
        request: &ControlRequest,
    ) -> io::Result<Option<CommittedControl>> {
        Ok(self.controls.previous(request)?.cloned())
    }
    /// Persist the request and state change atomically. Identical retries return the original receipt even after subsequent turns or closure. Authorization must precede this call, including retries.
    pub fn control(&mut self, request: ControlRequest) -> io::Result<CommittedControl> {
        if let Some(record) = self.controls.previous(&request)? {
            return Ok(record.clone());
        }
        let candidate = self.controls.prepare(&self.state, &request)?;
        let update = ControlUpdate {
            request,
            at: chrono::Utc::now(),
        };
        let receipt = self
            .journal
            .commit(&[FlowFact::Controlled(Box::new(update.clone()))])?;
        if candidate != self.state {
            self.publish_state(candidate.clone(), receipt.clone());
        } else {
            self.cursor = receipt.clone();
        }
        Ok(self.controls.apply(update, candidate, receipt))
    }
}
