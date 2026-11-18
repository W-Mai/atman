//! Lease-bound input admission and atomic invocation startup.

use std::io;

use atman_rt::{RunId, TurnId, TurnLease};
use uuid::Uuid;

use super::inbox::{
    AcceptedEnvelope, CommittedInput, DeliveryMode, InputContent, InputRecord, InputRecordRef,
    InputReservation, InputTarget,
};
use super::{FlowFact, FlowStore, FlowTransition, invalid};
use crate::event::AtmanUuid;
use crate::flow_store::position::FlowPosition;

impl FlowStore {
    /// Compare authored retry identity before source preparation, without loading its body.
    pub(crate) fn observe_authored_input(
        &self,
        operator: super::local_operator::LocalOperator,
        request_id: Uuid,
        authored: super::inbox::AuthoredInputFingerprint,
    ) -> io::Result<Option<super::inbox::Acceptance>> {
        if request_id.is_nil() {
            return Err(invalid("authored input requires a request identity"));
        }
        let sender = self.operator_input_sender(operator, None, request_id)?;
        self.inbox.previous_authored(&sender, request_id, authored)
    }

    /// Capture authored provenance in the original acceptance commit.
    pub(crate) fn accept_authored_input(
        &mut self,
        operator: super::local_operator::LocalOperator,
        mut request: super::inbox::DeliveryRequest,
        authored: super::inbox::AuthoredInputFingerprint,
        authorize_new: impl FnOnce(&super::inbox::DeliveryRequest) -> io::Result<()>,
    ) -> io::Result<super::inbox::Acceptance> {
        request.validate_authored_input()?;
        if request.recipient != self.descriptor.address {
            return Err(invalid("delivery targets another flow"));
        }
        request.sender =
            self.operator_input_sender(operator, Some(&request.sender), request.request_id)?;
        if let Some(previous) =
            self.observe_authored_input(operator, request.request_id, authored)?
        {
            return Ok(previous);
        }
        self.accept_input_with_authored(request, Some(authored), authorize_new)
    }

    /// Resolve the trusted namespace before the original immutable request fingerprint check.
    pub fn accept_operator_input(
        &mut self,
        operator: super::local_operator::LocalOperator,
        mut request: super::inbox::DeliveryRequest,
        authorize_new: impl FnOnce(&super::inbox::DeliveryRequest) -> io::Result<()>,
    ) -> io::Result<super::inbox::Acceptance> {
        request.sender =
            self.operator_input_sender(operator, Some(&request.sender), request.request_id)?;
        self.accept_input(request, authorize_new)
    }

    pub fn committed_inputs(&self) -> &[InputRecordRef] {
        self.inbox.inputs()
    }

    /// The last admitted invocation identity without reading its payload.
    pub fn last_invocation_ref(&self) -> Option<&InputRecordRef> {
        self.inbox.last_invocation()
    }

    /// Read the last admitted entry configuration from its original journal batch.
    pub fn last_invocation_input(&mut self) -> io::Result<Option<InputRecord>> {
        match self
            .last_invocation_ref()
            .map(|record| record.receipt.clone())
        {
            Some(receipt) => self.committed_input(&receipt),
            None => Ok(None),
        }
    }

    /// Read an admitted input using its complete verified commit identity.
    pub fn committed_input(&mut self, receipt: &FlowPosition) -> io::Result<Option<InputRecord>> {
        let Ok(index) = self
            .inbox
            .inputs()
            .binary_search_by_key(&receipt.sequence(), |record| record.receipt.sequence())
        else {
            return Ok(None);
        };
        let saved = &self.inbox.inputs()[index];
        if &saved.receipt != receipt {
            return Err(invalid(
                "input receipt does not match its committed identity",
            ));
        }
        let batch = self.journal.read_owned(receipt)?;
        let input = batch
            .facts
            .into_iter()
            .find_map(|fact| match fact {
                FlowFact::InputCommitted(input) => Some(*input),
                _ => None,
            })
            .ok_or_else(|| invalid("input is absent from its recorded batch"))?;
        Ok(Some(InputRecord {
            input,
            receipt: receipt.clone(),
        }))
    }

    pub(crate) fn command_for_input(
        &mut self,
        ids: &[Uuid],
    ) -> io::Result<Option<(Uuid, super::EntrySnapshot)>> {
        self.inbox
            .command_for_input(ids)?
            .map(|id| self.command_entry(id).map(|entry| (id, entry)))
            .transpose()
    }

    pub(crate) fn last_command_entry(&mut self) -> io::Result<Option<super::EntrySnapshot>> {
        self.inbox
            .last_command()
            .map(|id| self.command_entry(id))
            .transpose()
    }

    fn command_entry(&mut self, id: Uuid) -> io::Result<super::EntrySnapshot> {
        match self
            .inbox_entry(id)?
            .map(|entry| entry.envelope.request.payload)
        {
            Some(super::inbox::InboxPayload::Command { entry, .. }) => Ok(*entry),
            _ => Err(invalid("invocation command source is unavailable")),
        }
    }

    fn selected_inputs(
        &mut self,
        correlation: Option<Uuid>,
        filter: impl Fn(&super::inbox::InputEnvelope) -> bool,
        mut eligible: impl FnMut(&AcceptedEnvelope) -> bool,
    ) -> io::Result<std::collections::HashSet<Uuid>> {
        let ids: Vec<_> = self
            .inbox
            .candidates(correlation)
            .filter(|input| filter(input))
            .map(|input| input.id)
            .collect();
        let mut selected = std::collections::HashSet::new();
        for id in ids {
            let body = self
                .inbox_entry(id)?
                .ok_or_else(|| invalid("pending input body is unavailable"))?;
            if eligible(&body.envelope) {
                selected.insert(id);
                if correlation.is_some() {
                    break;
                }
            }
        }
        Ok(selected)
    }

    /// The host selects inputs compatible with the current invocation at an explicit safe point.
    pub fn reserve_context_input(
        &mut self,
        lease: &TurnLease<AtmanUuid>,
        eligible: impl FnMut(&AcceptedEnvelope) -> bool,
    ) -> io::Result<Option<InputReservation>> {
        if !self.state.can_receive_at_boundary(lease).map_err(invalid)? {
            return Err(invalid(
                "flow cannot reserve context input at this boundary",
            ));
        }
        let selected = self.selected_inputs(
            None,
            |input| input.request.delivery == DeliveryMode::AtBoundary,
            eligible,
        )?;
        Ok(self
            .inbox
            .reserve(|envelope| selected.contains(&envelope.id))
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
        if self.pending_history_operation().is_some() {
            return Ok(None);
        }
        let lease = self.state.clone().begin_turn(turn, run).map_err(invalid)?;
        let selected = self.selected_inputs(None, |_| true, eligible)?;
        Ok(self
            .inbox
            .reserve(|input| selected.contains(&input.id))
            .map(|batch| InputReservation {
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
        let selected = self.selected_inputs(Some(correlation), |_| true, eligible)?;
        Ok(self
            .inbox
            .reserve_reply(correlation, |input| selected.contains(&input.id))
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
    ) -> io::Result<FlowPosition> {
        if reservation.revision != self.state.revision()
            || !self.inbox.owns(&reservation.batch)
            || reservation.target != content.target()
        {
            return Err(invalid("stale or mismatched input reservation"));
        }
        let mut candidate = self.state.clone();
        let mut facts = Vec::new();
        if reservation.target == InputTarget::InvocationInput {
            self.validate_turn_admission()?;
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
            .commit_input(&reservation.batch, &input, || self.journal.commit(&facts))?;
        self.install_state(candidate);
        self.names.observe_input(&input.content, &self.descriptor);
        self.cursor = cursor.clone();
        self.refresh_projection();
        Ok(cursor)
    }
}
