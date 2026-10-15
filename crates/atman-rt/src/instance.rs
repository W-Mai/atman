//! Persistent instance state independent of executors, storage, and agent policy.
//!
//! Hosts serialize transitions, authorize controls, and persist them before exposing acknowledgements. A turn lease identifies the instance and execution generation; a callback from another instance or an earlier turn cannot mutate the current execution. Publishing an answer is not a state transition: the entry and its invocation resources must finish before `finish_turn`.

use alloc::vec::Vec;
use core::fmt;

use serde::{Deserialize, Serialize};

use crate::{FlowRef, RunId, TurnId};

/// Why an active invocation is waiting rather than executing.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    Effect,
    Reply,
    ExecutionGate,
}

/// Progress within one invocation, not the instance's lifetime or Goal state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    Running,
    Waiting(WaitReason),
    Finishing,
    Cancelling,
}

/// A host must explicitly recover an interrupted instance before scheduling work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Interruption {
    Cancelled,
    Failed,
    HostLost,
}

/// Entry outcome after invocation cleanup, independently of published answers.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnOutcome {
    Completed,
    Interrupted(Interruption),
}

/// Identifies one admitted entry invocation. Cloning a lease does not admit another run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TurnLease<I> {
    pub flow: FlowRef<I>,
    pub turn: TurnId<I>,
    pub run: RunId<I>,
    pub epoch: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActiveTurn<I> {
    pub lease: TurnLease<I>,
    pub phase: TurnPhase,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceActivity<I> {
    Idle,
    Active(ActiveTurn<I>),
    Interrupted {
        lease: TurnLease<I>,
        reason: Interruption,
    },
}

/// Closing retains an active lease until that invocation has actually exited.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum InstanceState<I> {
    Prepared,
    Open(InstanceActivity<I>),
    Closing { active: Option<ActiveTurn<I>> },
    Closed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InstanceError {
    InvalidTransition,
    Paused,
    StaleLease,
    RevisionExhausted,
}

impl fmt::Display for InstanceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidTransition => "invalid flow instance transition",
            Self::Paused => "flow execution is paused",
            Self::StaleLease => "flow turn lease is no longer active",
            Self::RevisionExhausted => "flow instance revision exhausted",
        })
    }
}

impl core::error::Error for InstanceError {}

/// State machine for one persistent instance; all mutable state has one owner.
///
/// Pause keys are opaque host-issued identities for independent controls, such as user or session pauses. Removing one key never releases another key. Goal policy is intentionally absent. This object does not acquire locks, schedule futures, authenticate callers, or claim durable storage.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlowInstance<I> {
    address: FlowRef<I>,
    state: InstanceState<I>,
    pauses: Vec<I>,
    revision: u64,
    epoch: u64,
}

impl<I: Clone + Eq> FlowInstance<I> {
    /// Prepare an instance without starting it or accepting inputs before activation.
    pub const fn prepared(address: FlowRef<I>) -> Self {
        Self {
            address,
            state: InstanceState::Prepared,
            pauses: Vec::new(),
            revision: 0,
            epoch: 0,
        }
    }

    pub fn address(&self) -> &FlowRef<I> {
        &self.address
    }

    pub fn state(&self) -> &InstanceState<I> {
        &self.state
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn pauses(&self) -> &[I] {
        &self.pauses
    }

    pub fn accepts_input(&self) -> bool {
        matches!(self.state, InstanceState::Open(_))
    }

    pub fn can_start_turn(&self) -> bool {
        self.pauses.is_empty() && matches!(self.state, InstanceState::Open(InstanceActivity::Idle))
    }

    /// Activate after the host has committed creation and required reply subscriptions.
    pub fn activate(&mut self) -> Result<(), InstanceError> {
        if self.state != InstanceState::Prepared {
            return Err(InstanceError::InvalidTransition);
        }
        self.advance_revision()?;
        self.state = InstanceState::Open(InstanceActivity::Idle);
        Ok(())
    }

    /// Admit one entry invocation. Hosts supply fresh turn and run identities.
    pub fn begin_turn(
        &mut self,
        turn: TurnId<I>,
        run: RunId<I>,
    ) -> Result<TurnLease<I>, InstanceError> {
        if !matches!(self.state, InstanceState::Open(InstanceActivity::Idle)) {
            return Err(InstanceError::InvalidTransition);
        }
        if !self.pauses.is_empty() {
            return Err(InstanceError::Paused);
        }
        let epoch = self
            .epoch
            .checked_add(1)
            .ok_or(InstanceError::RevisionExhausted)?;
        self.advance_revision()?;
        self.epoch = epoch;
        let lease = TurnLease {
            flow: self.address.clone(),
            turn,
            run,
            epoch,
        };
        self.state = InstanceState::Open(InstanceActivity::Active(ActiveTurn {
            lease: lease.clone(),
            phase: TurnPhase::Running,
        }));
        Ok(lease)
    }

    /// Inspect admission at an ordinary inbox boundary; wait replies have their own ownership path.
    pub fn can_receive_at_boundary(&self, lease: &TurnLease<I>) -> Result<bool, InstanceError> {
        let active = self.check_lease(lease)?;
        Ok(self.pauses.is_empty()
            && matches!(self.state, InstanceState::Open(_))
            && active.phase == TurnPhase::Running)
    }

    /// Admit a reply to an existing continuation without starting another turn.
    pub fn can_receive_reply(&self, lease: &TurnLease<I>) -> Result<bool, InstanceError> {
        let active = self.check_lease(lease)?;
        Ok(self.pauses.is_empty()
            && matches!(self.state, InstanceState::Open(_))
            && active.phase == TurnPhase::Waiting(WaitReason::Reply))
    }

    pub fn set_phase(
        &mut self,
        lease: &TurnLease<I>,
        phase: TurnPhase,
    ) -> Result<(), InstanceError> {
        let old = self.check_lease(lease)?.phase;
        if old == phase {
            return Ok(());
        }
        if matches!(old, TurnPhase::Cancelling)
            || (old == TurnPhase::Finishing && phase != TurnPhase::Cancelling)
            || !matches!(self.state, InstanceState::Open(_))
        {
            return Err(InstanceError::InvalidTransition);
        }
        if phase == TurnPhase::Running && !self.pauses.is_empty() {
            return Err(InstanceError::Paused);
        }
        if phase == TurnPhase::Waiting(WaitReason::ExecutionGate) && self.pauses.is_empty() {
            return Err(InstanceError::InvalidTransition);
        }
        self.advance_revision()?;
        if let InstanceState::Open(InstanceActivity::Active(active)) = &mut self.state {
            active.phase = phase;
        }
        Ok(())
    }

    /// Add an independently owned pause; an admitted invocation must acknowledge its safe point.
    pub fn pause(&mut self, key: I) -> Result<(), InstanceError> {
        self.require_open()?;
        if !self.pauses.contains(&key) {
            self.advance_revision()?;
            self.pauses.push(key);
        }
        Ok(())
    }

    /// Remove only the authorized pause key. An interrupted turn remains interrupted.
    pub fn resume(&mut self, key: &I) -> Result<(), InstanceError> {
        self.require_open()?;
        if let Some(index) = self.pauses.iter().position(|pause| pause == key) {
            self.advance_revision()?;
            self.pauses.remove(index);
        }
        Ok(())
    }

    /// Recover an interrupted instance after an explicit authorized decision, preserving pause keys.
    pub fn recover_interrupted(&mut self) -> Result<(), InstanceError> {
        if !matches!(
            self.state,
            InstanceState::Open(InstanceActivity::Interrupted { .. })
        ) {
            return Err(InstanceError::InvalidTransition);
        }
        self.advance_revision()?;
        self.state = InstanceState::Open(InstanceActivity::Idle);
        Ok(())
    }

    /// Finish after the entry and invocation resources exit; cancellation cannot become success.
    pub fn finish_turn(
        &mut self,
        lease: &TurnLease<I>,
        outcome: TurnOutcome,
    ) -> Result<TurnOutcome, InstanceError> {
        let active = self.check_lease(lease)?;
        let outcome = if active.phase == TurnPhase::Cancelling {
            TurnOutcome::Interrupted(Interruption::Cancelled)
        } else {
            outcome
        };
        self.advance_revision()?;
        self.state = if matches!(self.state, InstanceState::Closing { .. }) {
            InstanceState::Closing { active: None }
        } else {
            InstanceState::Open(match outcome {
                TurnOutcome::Completed => InstanceActivity::Idle,
                TurnOutcome::Interrupted(reason) => InstanceActivity::Interrupted {
                    lease: lease.clone(),
                    reason,
                },
            })
        };
        Ok(outcome)
    }

    /// Reject new work immediately; active execution and instance resources still need cleanup.
    pub fn begin_close(&mut self) -> Result<(), InstanceError> {
        if matches!(
            self.state,
            InstanceState::Closing { .. } | InstanceState::Closed
        ) {
            return Ok(());
        }
        self.advance_revision()?;
        let active = match &self.state {
            InstanceState::Open(InstanceActivity::Active(active)) => Some(ActiveTurn {
                lease: active.lease.clone(),
                phase: TurnPhase::Cancelling,
            }),
            _ => None,
        };
        self.state = InstanceState::Closing { active };
        Ok(())
    }

    /// Acknowledge instance-resource cleanup. A live entry prevents completion.
    pub fn finish_close(&mut self) -> Result<(), InstanceError> {
        if self.state == InstanceState::Closed {
            return Ok(());
        }
        if self.state != (InstanceState::Closing { active: None }) {
            return Err(InstanceError::InvalidTransition);
        }
        self.advance_revision()?;
        self.state = InstanceState::Closed;
        Ok(())
    }

    fn require_open(&self) -> Result<(), InstanceError> {
        if self.accepts_input() {
            Ok(())
        } else {
            Err(InstanceError::InvalidTransition)
        }
    }

    fn check_lease(&self, lease: &TurnLease<I>) -> Result<&ActiveTurn<I>, InstanceError> {
        match &self.state {
            InstanceState::Open(InstanceActivity::Active(active))
            | InstanceState::Closing {
                active: Some(active),
            } if &active.lease == lease => Ok(active),
            _ => Err(InstanceError::StaleLease),
        }
    }

    fn advance_revision(&mut self) -> Result<(), InstanceError> {
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or(InstanceError::RevisionExhausted)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FlowInstanceId;

    #[test]
    fn exhausted_revisions_and_epochs_leave_state_unchanged() {
        let mut flow = FlowInstance::prepared(FlowRef {
            session_id: 1,
            instance_id: FlowInstanceId::new(2),
        });
        flow.revision = u64::MAX;
        let before = flow.clone();
        assert_eq!(flow.activate(), Err(InstanceError::RevisionExhausted));
        assert_eq!(flow, before);
        flow.revision = 0;
        flow.activate().unwrap();
        flow.epoch = u64::MAX;
        let before = flow.clone();
        assert_eq!(
            flow.begin_turn(TurnId::new(3), RunId::new(4)),
            Err(InstanceError::RevisionExhausted)
        );
        assert_eq!(flow, before);
    }
}
