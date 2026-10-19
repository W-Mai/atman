//! Durable execution controls with actor-scoped retry receipts.

use std::{collections::HashMap, io};

use atman_rt::{FlowInstance, InstanceActivity, InstanceState, TurnPhase};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{FlowFact, FlowRef, FlowStore, FlowTransition, inbox::SenderIdentity, invalid};
use crate::{event::AtmanUuid, event_log::journal::CommitReceipt};

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
#[derive(Clone, Debug, PartialEq)]
pub struct CommittedControl {
    pub update: ControlUpdate,
    pub execution: FlowInstance<AtmanUuid>,
    pub receipt: CommitReceipt,
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
        receipt: CommitReceipt,
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
