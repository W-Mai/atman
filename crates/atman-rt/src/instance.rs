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

/// Data-only instance observation. Restoring validates owner, generation and state invariants.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InstanceSnapshot<I> {
    pub address: FlowRef<I>,
    pub state: InstanceState<I>,
    pub pauses: Vec<I>,
    pub revision: u64,
    pub epoch: u64,
}

impl<I: Clone + Eq> FlowInstance<I> {
    pub fn snapshot(&self) -> InstanceSnapshot<I> {
        InstanceSnapshot {
            address: self.address.clone(),
            state: self.state.clone(),
            pauses: self.pauses.clone(),
            revision: self.revision,
            epoch: self.epoch,
        }
    }

    /// Validate an observation without acquiring storage, scheduling or execution authority.
    pub fn restore(snapshot: InstanceSnapshot<I>) -> Result<Self, InstanceError> {
        if snapshot.epoch > snapshot.revision
            || snapshot
                .pauses
                .iter()
                .enumerate()
                .any(|(index, key)| snapshot.pauses[..index].contains(key))
        {
            return Err(InstanceError::InvalidTransition);
        }
        match &snapshot.state {
            InstanceState::Prepared
                if snapshot.revision != 0 || snapshot.epoch != 0 || !snapshot.pauses.is_empty() =>
            {
                return Err(InstanceError::InvalidTransition);
            }
            InstanceState::Prepared => {}
            _ if snapshot.revision == 0 => return Err(InstanceError::InvalidTransition),
            InstanceState::Closed if snapshot.revision < 2 => {
                return Err(InstanceError::InvalidTransition);
            }
            InstanceState::Closing {
                active: Some(active),
            } if active.phase != TurnPhase::Cancelling => {
                return Err(InstanceError::InvalidTransition);
            }
            _ => {}
        }
        let lease = match &snapshot.state {
            InstanceState::Open(InstanceActivity::Active(active))
            | InstanceState::Closing {
                active: Some(active),
            } => Some(&active.lease),
            InstanceState::Open(InstanceActivity::Interrupted { lease, .. }) => Some(lease),
            _ => None,
        };
        if lease.is_some_and(|lease| {
            lease.flow != snapshot.address
                || lease.epoch != snapshot.epoch
                || lease.epoch == 0
                || snapshot.revision <= snapshot.epoch
        }) {
            return Err(InstanceError::StaleLease);
        }
        Ok(Self {
            address: snapshot.address,
            state: snapshot.state,
            pauses: snapshot.pauses,
            revision: snapshot.revision,
            epoch: snapshot.epoch,
        })
    }

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

    /// Admit a reply to an existing continuation without starting another turn. Other branches may still be running or waiting for effects; the host validates correlation ownership independently.
    pub fn can_receive_reply(&self, lease: &TurnLease<I>) -> Result<bool, InstanceError> {
        let active = self.check_lease(lease)?;
        Ok(self.pauses.is_empty()
            && matches!(self.state, InstanceState::Open(_))
            && matches!(
                active.phase,
                TurnPhase::Running | TurnPhase::Waiting(WaitReason::Reply | WaitReason::Effect)
            ))
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
    fn observations_restore_only_matching_owner_and_execution_generation() {
        let mut flow = FlowInstance::prepared(FlowRef {
            session_id: 1,
            instance_id: FlowInstanceId::new(2),
        });
        assert_eq!(FlowInstance::restore(flow.snapshot()).unwrap(), flow);
        flow.activate().unwrap();
        let lease = flow.begin_turn(TurnId::new(3), RunId::new(4)).unwrap();
        flow.pause(9).unwrap();
        let snapshot = flow.snapshot();
        assert_eq!(FlowInstance::restore(snapshot.clone()).unwrap(), flow);
        let mut wrong = snapshot.clone();
        wrong.address.session_id = 5;
        assert_eq!(FlowInstance::restore(wrong), Err(InstanceError::StaleLease));
        let mut wrong = snapshot.clone();
        wrong.epoch += 1;
        assert!(FlowInstance::restore(wrong).is_err());
        let mut wrong = snapshot;
        wrong.pauses.push(9);
        assert!(FlowInstance::restore(wrong).is_err());
        flow.finish_turn(&lease, TurnOutcome::Interrupted(Interruption::HostLost))
            .unwrap();
        assert_eq!(FlowInstance::restore(flow.snapshot()).unwrap(), flow);
        flow.begin_close().unwrap();
        flow.finish_close().unwrap();
        assert_eq!(FlowInstance::restore(flow.snapshot()).unwrap(), flow);
    }

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
