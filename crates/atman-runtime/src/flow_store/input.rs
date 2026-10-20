//! Lease-bound input admission and atomic invocation startup.

use std::io;

use atman_rt::{RunId, TurnId, TurnLease};
use uuid::Uuid;

use super::inbox::{
    AcceptedEnvelope, CommittedInput, DeliveryMode, InputContent, InputRecord, InputReservation,
    InputTarget,
};
use super::{FlowFact, FlowStore, FlowTransition, invalid};
use crate::event::AtmanUuid;
use crate::event_log::journal::CommitReceipt;

impl FlowStore {
    pub fn committed_inputs(&self) -> &[InputRecord] {
        self.inbox.inputs()
    }

    /// The last admitted entry configuration, reconstructed by the same live/replay path.
    pub fn last_invocation_input(&self) -> Option<&InputRecord> {
        self.inbox.last_invocation()
    }

    pub(crate) fn command_for_input(
        &self,
        ids: &[Uuid],
    ) -> io::Result<Option<(Uuid, &super::EntrySnapshot)>> {
        self.inbox.command_for_input(ids)
    }

    pub(crate) fn last_command_entry(&self) -> io::Result<Option<&super::EntrySnapshot>> {
        self.inbox
            .last_command()
            .map(|id| self.inbox.command_entry(id))
            .transpose()
    }

    /// The host selects inputs compatible with the current invocation at an explicit safe point.
    pub fn reserve_context_input(
        &mut self,
        lease: &TurnLease<AtmanUuid>,
        mut eligible: impl FnMut(&AcceptedEnvelope) -> bool,
    ) -> io::Result<Option<InputReservation>> {
        if !self.state.can_receive_at_boundary(lease).map_err(invalid)? {
            return Err(invalid(
                "flow cannot reserve context input at this boundary",
            ));
        }
        Ok(self
            .inbox
            .reserve(|envelope| {
                envelope.request.delivery == DeliveryMode::AtBoundary && eligible(envelope)
            })
            .map(|batch| InputReservation {
                lease: lease.clone(),
                revision: self.state.revision(),
                target: InputTarget::Context,
                batch,
            }))
    }

    /// Reserve while idle; the candidate turn does not exist until its actual input is committed.
    pub fn reserve_invocation_input(
        &mut self,
        turn: TurnId<AtmanUuid>,
        run: RunId<AtmanUuid>,
        eligible: impl FnMut(&AcceptedEnvelope) -> bool,
    ) -> io::Result<Option<InputReservation>> {
        let lease = self.state.clone().begin_turn(turn, run).map_err(invalid)?;
        Ok(self.inbox.reserve(eligible).map(|batch| InputReservation {
            lease,
            revision: self.state.revision(),
            target: InputTarget::InvocationInput,
            batch,
        }))
    }

    pub fn release_input(&mut self, reservation: &InputReservation) -> bool {
        self.inbox.release(&reservation.batch)
    }

    /// Select the oldest matching reply from the same queue used by ordinary input.
    pub fn reserve_reply_input(
        &mut self,
        lease: &TurnLease<AtmanUuid>,
        correlation: Uuid,
        eligible: impl FnMut(&AcceptedEnvelope) -> bool,
    ) -> io::Result<Option<InputReservation>> {
        self.inbox.validate_reply(&self.state, lease, correlation)?;
        Ok(self
            .inbox
            .reserve_reply(correlation, eligible)
            .map(|batch| InputReservation {
                lease: lease.clone(),
                revision: self.state.revision(),
                target: InputTarget::WaitingContinuation { correlation },
                batch,
            }))
    }

    /// Persist actual input and consumption together; invocation startup belongs to the same commit.
    ///
    /// The host prepares provenance and invocation configuration before calling this method. No model request or entry effect may run before it succeeds. A storage error requires reopening and reconciling the journal, not retrying execution.
    pub fn commit_input(
        &mut self,
        reservation: &InputReservation,
        content: InputContent,
    ) -> io::Result<CommitReceipt> {
        if reservation.revision != self.state.revision()
            || !self.inbox.owns(&reservation.batch)
            || reservation.target != content.target()
        {
            return Err(invalid("stale or mismatched input reservation"));
        }
        let mut candidate = self.state.clone();
        let mut facts = Vec::new();
        if reservation.target == InputTarget::InvocationInput {
            let lease = candidate
                .begin_turn(
                    reservation.lease.turn.clone(),
                    reservation.lease.run.clone(),
                )
                .map_err(invalid)?;
            if lease != reservation.lease {
                return Err(invalid("stale invocation input lease"));
            }
            facts.push(FlowFact::Transition(FlowTransition::TurnStarted {
                turn: lease.turn,
                run: lease.run,
            }));
        }
        let input = CommittedInput {
            lease: reservation.lease.clone(),
            message_ids: reservation.message_ids().to_vec(),
            content,
        };
        self.inbox.validate_input(&candidate, &input)?;
        facts.push(FlowFact::InputCommitted(Box::new(input.clone())));
        FlowFact::validate_batch(&facts)?;
        let cursor = self
            .inbox
            .commit_input(&reservation.batch, input, || self.journal.commit(&facts))?;
        self.state = candidate;
        self.cursor = cursor.clone();
        self.refresh_projection();
        Ok(cursor)
    }
}
